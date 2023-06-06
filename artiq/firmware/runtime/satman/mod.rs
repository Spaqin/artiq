#![feature(never_type, panic_info_message, llvm_asm, default_alloc_error_handler)]
#![no_std]

#[macro_use]
extern crate log;
#[macro_use]
extern crate board_misoc;
extern crate board_artiq;
extern crate riscv;
extern crate alloc;
extern crate proto_artiq;

use core::convert::TryFrom;
use board_misoc::{csr, ident, clock, uart_logger, i2c, pmp};
#[cfg(has_si5324)]
use board_artiq::si5324;
use board_artiq::{spi, drtioaux};
use board_artiq::drtio_routing;
use proto_artiq::drtioaux_proto::ANALYZER_MAX_SIZE;
use riscv::register::{mcause, mepc, mtval};
use dma::Manager as DmaManager;
use analyzer::Analyzer;

#[global_allocator]
static mut ALLOC: alloc_list::ListAlloc = alloc_list::EMPTY;

mod repeater;
mod dma;
mod analyzer;

fn drtiosat_reset(reset: bool) {
    unsafe {
        csr::drtiosat::reset_write(if reset { 1 } else { 0 });
    }
}

fn drtiosat_reset_phy(reset: bool) {
    unsafe {
        csr::drtiosat::reset_phy_write(if reset { 1 } else { 0 });
    }
}

fn drtiosat_link_rx_up() -> bool {
    unsafe {
        csr::drtiosat::rx_up_read() == 1
    }
}

fn drtiosat_tsc_loaded() -> bool {
    unsafe {
        let tsc_loaded = csr::drtiosat::tsc_loaded_read() == 1;
        if tsc_loaded {
            csr::drtiosat::tsc_loaded_write(1);
        }
        tsc_loaded
    }
}


#[cfg(has_drtio_routing)]
macro_rules! forward {
    ($routing_table:expr, $destination:expr, $rank:expr, $repeaters:expr, $packet:expr) => {{
        let hop = $routing_table.0[$destination as usize][$rank as usize];
        if hop != 0 {
            let repno = (hop - 1) as usize;
            if repno < $repeaters.len() {
                return $repeaters[repno].aux_forward($packet);
            } else {
                return Err(drtioaux::Error::RoutingError);
            }
        }
    }}
}

#[cfg(not(has_drtio_routing))]
macro_rules! forward {
    ($routing_table:expr, $destination:expr, $rank:expr, $repeaters:expr, $packet:expr) => {}
}

fn process_aux_packet(_manager: &mut DmaManager, analyzer: &mut Analyzer, _repeaters: &mut [repeater::Repeater],
        _routing_table: &mut drtio_routing::RoutingTable, _rank: &mut u8,
        packet: drtioaux::Packet) -> Result<(), drtioaux::Error<!>> {
    // In the code below, *_chan_sel_write takes an u8 if there are fewer than 256 channels,
    // and u16 otherwise; hence the `as _` conversion.
    match packet {
        drtioaux::Packet::EchoRequest =>
            drtioaux::send(0, &drtioaux::Packet::EchoReply),
        drtioaux::Packet::ResetRequest => {
            info!("resetting RTIO");
            drtiosat_reset(true);
            clock::spin_us(100);
            drtiosat_reset(false);
            for rep in _repeaters.iter() {
                if let Err(e) = rep.rtio_reset() {
                    error!("failed to issue RTIO reset ({})", e);
                }
            }
            drtioaux::send(0, &drtioaux::Packet::ResetAck)
        },

        drtioaux::Packet::DestinationStatusRequest { destination: _destination } => {
            #[cfg(has_drtio_routing)]
            let hop = _routing_table.0[_destination as usize][*_rank as usize];
            #[cfg(not(has_drtio_routing))]
            let hop = 0;

            if hop == 0 {
                let errors;
                unsafe {
                    errors = csr::drtiosat::rtio_error_read();
                }
                if errors & 1 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::sequence_error_channel_read();
                        csr::drtiosat::rtio_error_write(1);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationSequenceErrorReply { channel })?;
                } else if errors & 2 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::collision_channel_read();
                        csr::drtiosat::rtio_error_write(2);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationCollisionReply { channel })?;
                } else if errors & 4 != 0 {
                    let channel;
                    unsafe {
                        channel = csr::drtiosat::busy_channel_read();
                        csr::drtiosat::rtio_error_write(4);
                    }
                    drtioaux::send(0,
                        &drtioaux::Packet::DestinationBusyReply { channel })?;
                }
                else {
                    drtioaux::send(0, &drtioaux::Packet::DestinationOkReply)?;
                }
            }

            #[cfg(has_drtio_routing)]
            {
                if hop != 0 {
                    let hop = hop as usize;
                    if hop <= csr::DRTIOREP.len() {
                        let repno = hop - 1;
                        match _repeaters[repno].aux_forward(&drtioaux::Packet::DestinationStatusRequest {
                            destination: _destination
                        }) {
                            Ok(()) => (),
                            Err(drtioaux::Error::LinkDown) => drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?,
                            Err(e) => {
                                drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?;
                                error!("aux error when handling destination status request: {}", e);
                            },
                        }
                    } else {
                        drtioaux::send(0, &drtioaux::Packet::DestinationDownReply)?;
                    }
                }
            }

            Ok(())
        }

        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetPath { destination, hops } => {
            _routing_table.0[destination as usize] = hops;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_path(destination, &hops) {
                    error!("failed to set path ({})", e);
                }
            }
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }
        #[cfg(has_drtio_routing)]
        drtioaux::Packet::RoutingSetRank { rank } => {
            *_rank = rank;
            drtio_routing::interconnect_enable_all(_routing_table, rank);

            let rep_rank = rank + 1;
            for rep in _repeaters.iter() {
                if let Err(e) = rep.set_rank(rep_rank) {
                    error!("failed to set rank ({})", e);
                }
            }

            info!("rank: {}", rank);
            info!("routing table: {}", _routing_table);

            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }

        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetPath { destination: _, hops: _ } => {
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }
        #[cfg(not(has_drtio_routing))]
        drtioaux::Packet::RoutingSetRank { rank: _ } => {
            drtioaux::send(0, &drtioaux::Packet::RoutingAck)
        }

        drtioaux::Packet::MonitorRequest { destination: _destination, channel, probe } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let value;
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::mon_chan_sel_write(channel as _);
                csr::rtio_moninj::mon_probe_sel_write(probe);
                csr::rtio_moninj::mon_value_update_write(1);
                value = csr::rtio_moninj::mon_value_read() as u64;
            }
            #[cfg(not(has_rtio_moninj))]
            {
                value = 0;
            }
            let reply = drtioaux::Packet::MonitorReply { value: value };
            drtioaux::send(0, &reply)
        },
        drtioaux::Packet::InjectionRequest { destination: _destination, channel, overrd, value } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                csr::rtio_moninj::inj_value_write(value);
            }
            Ok(())
        },
        drtioaux::Packet::InjectionStatusRequest { destination: _destination, channel, overrd } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let value;
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                value = csr::rtio_moninj::inj_value_read();
            }
            #[cfg(not(has_rtio_moninj))]
            {
                value = 0;
            }
            drtioaux::send(0, &drtioaux::Packet::InjectionStatusReply { value: value })
        },

        drtioaux::Packet::I2cStartRequest { destination: _destination, busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = i2c::start(busno).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cRestartRequest { destination: _destination, busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = i2c::restart(busno).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cStopRequest { destination: _destination, busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = i2c::stop(busno).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::I2cWriteRequest { destination: _destination, busno, data } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            match i2c::write(busno, data) {
                Ok(ack) => drtioaux::send(0,
                    &drtioaux::Packet::I2cWriteReply { succeeded: true, ack: ack }),
                Err(_) => drtioaux::send(0,
                    &drtioaux::Packet::I2cWriteReply { succeeded: false, ack: false })
            }
        }
        drtioaux::Packet::I2cReadRequest { destination: _destination, busno, ack } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            match i2c::read(busno, ack) {
                Ok(data) => drtioaux::send(0,
                    &drtioaux::Packet::I2cReadReply { succeeded: true, data: data }),
                Err(_) => drtioaux::send(0,
                    &drtioaux::Packet::I2cReadReply { succeeded: false, data: 0xff })
            }
        }
        drtioaux::Packet::I2cSwitchSelectRequest { destination: _destination, busno, address, mask } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = i2c::switch_select(busno, address, mask).is_ok();
            drtioaux::send(0, &drtioaux::Packet::I2cBasicReply { succeeded: succeeded })
        }

        drtioaux::Packet::SpiSetConfigRequest { destination: _destination, busno, flags, length, div, cs } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = spi::set_config(busno, flags, length, div, cs).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::SpiBasicReply { succeeded: succeeded })
        },
        drtioaux::Packet::SpiWriteRequest { destination: _destination, busno, data } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = spi::write(busno, data).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::SpiBasicReply { succeeded: succeeded })
        }
        drtioaux::Packet::SpiReadRequest { destination: _destination, busno } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            match spi::read(busno) {
                Ok(data) => drtioaux::send(0,
                    &drtioaux::Packet::SpiReadReply { succeeded: true, data: data }),
                Err(_) => drtioaux::send(0,
                    &drtioaux::Packet::SpiReadReply { succeeded: false, data: 0 })
            }
        }

        drtioaux::Packet::AnalyzerHeaderRequest { destination: _destination } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let header = analyzer.get_header();
            drtioaux::send(0, &drtioaux::Packet::AnalyzerHeader {
                total_byte_count: header.total_byte_count,
                sent_bytes: header.sent_bytes,
                overflow_occurred: header.overflow,
            })
        }

        drtioaux::Packet::AnalyzerDataRequest { destination: _destination } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let mut data_slice: [u8; ANALYZER_MAX_SIZE] = [0; ANALYZER_MAX_SIZE];
            let meta = analyzer.get_data(&mut data_slice);
            drtioaux::send(0, &drtioaux::Packet::AnalyzerData {
                last: meta.last,
                length: meta.len,
                data: data_slice,
            })
        }

        #[cfg(has_rtio_dma)]
        drtioaux::Packet::DmaAddTraceRequest { destination: _destination, id, last, length, trace } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = _manager.add(id, last, &trace, length as usize).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::DmaAddTraceReply { succeeded: succeeded })
        }
        #[cfg(has_rtio_dma)]
        drtioaux::Packet::DmaRemoveTraceRequest { destination: _destination, id } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = _manager.erase(id).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::DmaRemoveTraceReply { succeeded: succeeded })
        }
        #[cfg(has_rtio_dma)]
        drtioaux::Packet::DmaPlaybackRequest { destination: _destination, id, timestamp } => {
            forward!(_routing_table, _destination, *_rank, _repeaters, &packet);
            let succeeded = _manager.playback(id, timestamp).is_ok();
            drtioaux::send(0,
                &drtioaux::Packet::DmaPlaybackReply { succeeded: succeeded })
        }

        _ => {
            warn!("received unexpected aux packet");
            Ok(())
        }
    }
}

fn process_aux_packets(dma_manager: &mut DmaManager, analyzer: &mut Analyzer,
        repeaters: &mut [repeater::Repeater],
        routing_table: &mut drtio_routing::RoutingTable, rank: &mut u8) {
    let result =
        drtioaux::recv(0).and_then(|packet| {
            if let Some(packet) = packet {
                process_aux_packet(dma_manager, analyzer, repeaters, routing_table, rank, packet)
            } else {
                Ok(())
            }
        });
    match result {
        Ok(()) => (),
        Err(e) => warn!("aux packet error ({})", e)
    }
}

fn drtiosat_process_errors() {
    let errors;
    unsafe {
        errors = csr::drtiosat::protocol_error_read();
    }
    if errors & 1 != 0 {
        error!("received packet of an unknown type");
    }
    if errors & 2 != 0 {
        error!("received truncated packet");
    }
    if errors & 4 != 0 {
        let destination;
        unsafe {
            destination = csr::drtiosat::buffer_space_timeout_dest_read();
        }
        error!("timeout attempting to get buffer space from CRI, destination=0x{:02x}", destination)
    }
    if errors & 8 != 0 {
        let channel;
        let timestamp_event;
        let timestamp_counter;
        unsafe {
            channel = csr::drtiosat::underflow_channel_read();
            timestamp_event = csr::drtiosat::underflow_timestamp_event_read() as i64;
            timestamp_counter = csr::drtiosat::underflow_timestamp_counter_read() as i64;
        }
        error!("write underflow, channel={}, timestamp={}, counter={}, slack={}",
               channel, timestamp_event, timestamp_counter, timestamp_event-timestamp_counter);
    }
    if errors & 16 != 0 {
        error!("write overflow");
    }
    unsafe {
        csr::drtiosat::protocol_error_write(errors);
    }
}

fn hardware_tick(ts: &mut u64) {
    let now = clock::get_ms();
    if now > *ts {
        #[cfg(has_grabber)]
        board_artiq::grabber::tick();
        *ts = now + 200;
    }
}

fn satman_thread(io: &Io) {
    #[cfg(has_drtio_routing)]
    let mut repeaters = [repeater::Repeater::default(); csr::DRTIOREP.len()];
    #[cfg(not(has_drtio_routing))]
    let mut repeaters = [repeater::Repeater::default(); 0];
    for i in 0..repeaters.len() {
        repeaters[i] = repeater::Repeater::new(i as u8);
    } 
    let mut routing_table = drtio_routing::RoutingTable::default_empty();
    let mut rank = 1;

    let mut hardware_tick_ts = 0;

    loop {
        while !drtiosat_link_rx_up() {
            drtiosat_process_errors();
            for rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank);
            }
            hardware_tick(&mut hardware_tick_ts);
            io.relinquish().unwrap();
        }

        info!("uplink is up, switching to recovered clock");
        #[cfg(has_si5324)]
        {
            si5324::siphaser::select_recovered_clock(true).expect("failed to switch clocks");
            si5324::siphaser::calibrate_skew().expect("failed to calibrate skew");
        }

        // DMA manager created here, so when link is dropped, all DMA traces
        // are cleared out for a clean slate on subsequent connections,
        // without a manual intervention.
        let mut dma_manager = DmaManager::new();

        // Reset the analyzer as well.
        let mut analyzer = Analyzer::new();

        drtioaux::reset(0);
        drtiosat_reset(false);
        drtiosat_reset_phy(false);

        while drtiosat_link_rx_up() {
            drtiosat_process_errors();
            process_aux_packets(&mut dma_manager, &mut analyzer, &mut repeaters, &mut routing_table, &mut rank);
            for rep in repeaters.iter_mut() {
                rep.service(&routing_table, rank);
            }
            hardware_tick(&mut hardware_tick_ts);
            if drtiosat_tsc_loaded() {
                info!("TSC loaded from uplink");
                for rep in repeaters.iter() {
                    if let Err(e) = rep.sync_tsc() {
                        error!("failed to sync TSC ({})", e);
                    }
                }
                if let Err(e) = drtioaux::send(0, &drtioaux::Packet::TSCAck) {
                    error!("aux packet error: {}", e);
                }
            }
            if let Some(status) = dma_manager.check_state() {
                info!("playback done, error: {}, channel: {}, timestamp: {}", status.error, status.channel, status.timestamp);
                if let Err(e) = drtioaux::send(0, &drtioaux::Packet::DmaPlaybackStatus { 
                    destination: rank, id: status.id, error: status.error, channel: status.channel, timestamp: status.timestamp }) {
                    error!("error sending DMA playback status: {}", e);
                }
            }
            io.relinquish().unwrap();
        }

        drtiosat_reset_phy(true);
        drtiosat_reset(true);
        drtiosat_tsc_loaded();
        info!("uplink is down, switching to local oscillator clock");
        #[cfg(has_si5324)]
        si5324::siphaser::select_recovered_clock(false).expect("failed to switch clocks");
    }
}

pub fn start(io: &Io) -> i32 {
    info!("ARTIQ satellite manager starting...");

    init_rtio_crg();

    io.spawn(16384, satman_thread);
}