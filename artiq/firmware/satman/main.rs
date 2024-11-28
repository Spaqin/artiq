#![feature(never_type, panic_info_message, llvm_asm, default_alloc_error_handler)]
#![no_std]

#[macro_use]
extern crate log;
#[macro_use]
extern crate board_misoc;
extern crate board_artiq;
extern crate logger_artiq;
extern crate riscv;
extern crate alloc;
extern crate proto_artiq;
extern crate byteorder;
extern crate crc;
extern crate cslice;
extern crate io;
extern crate eh;

use core::convert::TryFrom;
use board_misoc::{csr, ident, clock, config, i2c, pmp};
#[cfg(has_si5324)]
use board_artiq::si5324;
#[cfg(has_si549)]
use board_artiq::si549;
#[cfg(soc_platform = "kasli")]
use board_misoc::irq;
use board_misoc::{boot, spiflash};
use board_artiq::{spi, drtioaux};
#[cfg(soc_platform = "efc")]
use board_artiq::ad9117;
use proto_artiq::drtioaux_proto::{DRTIO_PAYLOAD_SIZE, DRTIO_ID_PAYLOAD_SIZE};
#[cfg(has_drtio_eem)]
use board_artiq::drtio_eem;
use riscv::register::{mcause, mepc, mtval};
use dma::Manager as DmaManager;
use kernel::Manager as KernelManager;
use mgmt::Manager as CoreManager;
use analyzer::Analyzer;

#[global_allocator]
static mut ALLOC: alloc_list::ListAlloc = alloc_list::EMPTY;

mod repeater;
mod aux;
mod dma;
mod analyzer;
mod kernel;
mod cache;
mod mgmt;

pub fn drtiosat_reset(reset: bool) {
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

fn toggle_sed_spread(val: u8) {
    unsafe { csr::drtiosat::sed_spread_enable_write(val); }
}

#[derive(Clone, Copy)]
pub enum RtioMaster {
    Drtio,
    Dma,
    Kernel
}

pub fn cricon_select(master: RtioMaster) {
    let val = match master {
        RtioMaster::Drtio => 0,
        RtioMaster::Dma => 1,
        RtioMaster::Kernel => 2
    };
    unsafe {
        csr::cri_con::selected_write(val);
    }
}

pub fn cricon_read() -> RtioMaster {
    let val = unsafe { csr::cri_con::selected_read() };
    match val {
        0 => RtioMaster::Drtio,
        1 => RtioMaster::Dma,
        2 => RtioMaster::Kernel,
        _ => unreachable!()
    }
}

fn process_aux_packet(dmamgr: &mut DmaManager, analyzer: &mut Analyzer, kernelmgr: &mut KernelManager,
        _repeaters: &mut [repeater::Repeater], aux_mgr: &mut aux::AuxManager, coremgr: &mut CoreManager,
        packet: &drtioaux::Payload, transaction_id: u8, source: u8) {
    macro_rules! respond {
        ( $packet:expr ) => {
            aux_mgr.respond(transaction_id, source, &$packet)
        }
    }
    match *packet {
        drtioaux::Payload::DestinationStatusRequest => {
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
                respond!(drtioaux::Payload::DestinationSequenceErrorReply { channel })
            } else if errors & 2 != 0 {
                let channel;
                unsafe {
                    channel = csr::drtiosat::collision_channel_read();
                    csr::drtiosat::rtio_error_write(2);
                }
                respond!(drtioaux::Payload::DestinationCollisionReply { channel })
            } else if errors & 4 != 0 {
                let channel;
                unsafe {
                    channel = csr::drtiosat::busy_channel_read();
                    csr::drtiosat::rtio_error_write(4);
                }
                respond!(drtioaux::Payload::DestinationBusyReply { channel })
            } else {
                respond!(drtioaux::Payload::DestinationOkReply)
            }
        }

        #[cfg(not(has_drtio_routing))]
        drtioaux::Payload::RoutingSetPath { destination: _, hops: _ } => {
            drtioaux::send(0, &drtioaux::Payload::RoutingAck)
        }
        #[cfg(not(has_drtio_routing))]
        drtioaux::Payload::RoutingSetRank { rank: _ } => {
            drtioaux::send(0, &drtioaux::Payload::RoutingAck)
        }

        drtioaux::Payload::MonitorRequest { channel, probe } => {
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
            respond!(drtioaux::Payload::MonitorReply { value: value })
        },
        drtioaux::Payload::InjectionRequest { channel, overrd, value } => {
            #[cfg(has_rtio_moninj)]
            unsafe {
                csr::rtio_moninj::inj_chan_sel_write(channel as _);
                csr::rtio_moninj::inj_override_sel_write(overrd);
                csr::rtio_moninj::inj_value_write(value);
            }
        },
        drtioaux::Payload::InjectionStatusRequest { channel, overrd } => {
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
            respond!(drtioaux::Payload::InjectionStatusReply { value: value })
        },

        drtioaux::Payload::I2cStartRequest { busno } => {
            let succeeded = i2c::start(busno).is_ok();
            respond!(drtioaux::Payload::I2cBasicReply { succeeded })
        }
        drtioaux::Payload::I2cRestartRequest { busno } => {
            let succeeded = i2c::restart(busno).is_ok();
            respond!(drtioaux::Payload::I2cBasicReply { succeeded })
        }
        drtioaux::Payload::I2cStopRequest { busno } => {
            let succeeded = i2c::stop(busno).is_ok();
            respond!(drtioaux::Payload::I2cBasicReply { succeeded })
        }
        drtioaux::Payload::I2cWriteRequest { busno, data } => {
            match i2c::write(busno, data) {
                Ok(ack) => respond!(
                    drtioaux::Payload::I2cWriteReply { succeeded: true, ack: ack }),
                Err(_) => respond!(
                    drtioaux::Payload::I2cWriteReply { succeeded: false, ack: false })
            }
        }
        drtioaux::Payload::I2cReadRequest { busno, ack } => {
            match i2c::read(busno, ack) {
                Ok(data) => respond!(
                    drtioaux::Payload::I2cReadReply { succeeded: true, data: data }),
                Err(_) => respond!(
                    drtioaux::Payload::I2cReadReply { succeeded: false, data: 0xff })
            }
        }
        drtioaux::Payload::I2cSwitchSelectRequest { busno, address, mask } => {
            let succeeded = i2c::switch_select(busno, address, mask).is_ok();
            respond!(drtioaux::Payload::I2cBasicReply { succeeded })
        }

        drtioaux::Payload::SpiSetConfigRequest { busno, flags, length, div, cs } => {
            let succeeded = spi::set_config(busno, flags, length, div, cs).is_ok();
            respond!(
                drtioaux::Payload::SpiBasicReply { succeeded })
        },
        drtioaux::Payload::SpiWriteRequest { busno, data } => {
            let succeeded = spi::write(busno, data).is_ok();
            respond!(
                drtioaux::Payload::SpiBasicReply { succeeded })
        }
        drtioaux::Payload::SpiReadRequest { busno } => {
            match spi::read(busno) {
                Ok(data) => respond!(
                    drtioaux::Payload::SpiReadReply { succeeded: true, data: data }),
                Err(_) => respond!(
                    drtioaux::Payload::SpiReadReply { succeeded: false, data: 0 })
            }
        }

        drtioaux::Payload::AnalyzerHeaderRequest => {
            let header = analyzer.get_header();
            respond!(drtioaux::Payload::AnalyzerHeader {
                total_byte_count: header.total_byte_count,
                sent_bytes: header.sent_bytes,
                overflow_occurred: header.overflow,
            })
        }

        drtioaux::Payload::AnalyzerDataRequest => {
            let mut data_slice: [u8; DRTIO_PAYLOAD_SIZE] = [0; DRTIO_PAYLOAD_SIZE];
            let meta = analyzer.get_data(&mut data_slice);
            respond!(drtioaux::Payload::AnalyzerData {
                last: meta.last,
                length: meta.len,
                data: data_slice,
            })
        }

        drtioaux::Payload::DmaAddTraceRequest { id, status, length, trace } => {
            let succeeded = dmamgr.add(source, id, status, &trace, length as usize).is_ok();
            respond!(drtioaux::Payload::DmaAddTraceReply { 
                id: id, succeeded: succeeded 
            })
        }
        drtioaux::Payload::DmaRemoveTraceRequest { id } => {
            let succeeded = dmamgr.erase(source, id).is_ok();
            respond!(drtioaux::Payload::DmaRemoveTraceReply { 
                succeeded: succeeded 
            })
        }
        drtioaux::Payload::DmaPlaybackRequest { id, timestamp } => {
            // no DMA with a running kernel
            let succeeded = !kernelmgr.is_running() && dmamgr.playback(source, id, timestamp).is_ok();
            respond!(drtioaux::Payload::DmaPlaybackReply { succeeded })
        }
        drtioaux::Payload::DmaPlaybackStatus { id, error, channel, timestamp } => {
            dmamgr.remote_finished(kernelmgr, id, error, channel, timestamp);
        }

        drtioaux::Payload::SubkernelAddDataRequest { id, status, length, data } => {
            let succeeded = kernelmgr.add(id, status, &data, length as usize).is_ok();
            respond!(drtioaux::Payload::SubkernelAddDataReply { succeeded })
        }
        drtioaux::Payload::SubkernelLoadRunRequest { id, run, timestamp } => {
            let mut succeeded = kernelmgr.load(id).is_ok();
            // allow preloading a kernel with delayed run
            if run {
                if dmamgr.running() {
                    // cannot run kernel while DDMA is running
                    succeeded = false;
                } else {
                    succeeded |= kernelmgr.run(source, id, timestamp).is_ok();
                }
            }
            respond!(drtioaux::Payload::SubkernelLoadRunReply { succeeded })
        }
        drtioaux::Payload::SubkernelFinished { id, with_exception, exception_src } => {
            kernelmgr.remote_subkernel_finished(id, with_exception, exception_src);
        }
        drtioaux::Payload::SubkernelExceptionRequest => {
            let mut data_slice: [u8; DRTIO_PAYLOAD_SIZE] = [0; DRTIO_PAYLOAD_SIZE];
            let meta = kernelmgr.exception_get_slice(&mut data_slice);
            respond!(drtioaux::Payload::SubkernelException {
                last: meta.status.is_last(),
                length: meta.len,
                data: data_slice,
            })
        }
        drtioaux::Payload::SubkernelMessage { id, status, length, data } => {
            kernelmgr.message_handle_incoming(status, length as usize, id, &data);
        }

        drtioaux::Payload::CoreMgmtGetLogRequest { clear } => {
            let mut data_slice = [0; DRTIO_PAYLOAD_SIZE];
            if let Ok(meta) = coremgr.log_get_slice(&mut data_slice, clear) {
                respond!(drtioaux::Payload::CoreMgmtGetLogReply {
                    last: meta.status.is_last(),
                    length: meta.len as u16,
                    data: data_slice,
                })
            } else {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: false })
            }
        }
        drtioaux::Payload::CoreMgmtClearLogRequest => {
            respond!(&drtioaux::Payload::CoreMgmtReply { succeeded: mgmt::clear_log().is_ok() })
        }
        drtioaux::Payload::CoreMgmtSetLogLevelRequest { log_level } => {
            if let Ok(level_filter) = mgmt::byte_to_level_filter(log_level) {
                info!("changing log level to {}", level_filter);
                log::set_max_level(level_filter);
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true })
            } else {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: false })
            }
        }
        drtioaux::Payload::CoreMgmtSetUartLogLevelRequest { log_level } => {
            if let Ok(level_filter) = mgmt::byte_to_level_filter(log_level) {
                info!("changing UART log level to {}", level_filter);
                logger_artiq::BufferLogger::with(|logger|
                    logger.set_uart_log_level(level_filter));
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true })
            } else {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: false })
            }
        }
        drtioaux::Payload::CoreMgmtConfigReadRequest {
            length,
            key,
        } => {
            let mut value_slice = [0; DRTIO_PAYLOAD_SIZE];

            let key_slice = &key[..length as usize];
            if !key_slice.is_ascii() {
                error!("invalid key");
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: false })
            } else {
                let key = core::str::from_utf8(key_slice).unwrap();
                if coremgr.fetch_config_value(key).is_ok() {
                    let meta = coremgr.get_config_value_slice(&mut value_slice);
                    respond!(
                        drtioaux::Payload::CoreMgmtConfigReadReply {
                            length: meta.len as u16,
                            last: meta.status.is_last(),
                            value: value_slice,
                        })
                } else {
                    respond!(drtioaux::Payload::CoreMgmtReply { succeeded: false })
                }
            }
        }
        drtioaux::Payload::CoreMgmtConfigReadContinue => {
            let mut value_slice = [0; DRTIO_PAYLOAD_SIZE];
            let meta = coremgr.get_config_value_slice(&mut value_slice);
            respond!(drtioaux::Payload::CoreMgmtConfigReadReply {
                length: meta.len as u16,
                last: meta.status.is_last(),
                value: value_slice,
            })
        }
        drtioaux::Payload::CoreMgmtConfigWriteRequest { last, length, data }  => {
            coremgr.add_config_data(&data, length as usize);
            if last {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: coremgr.write_config() })
            } else {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true })
            }
        }
        drtioaux::Payload::CoreMgmtConfigRemoveRequest { length, key } => {
            let key = core::str::from_utf8(&key[..length as usize]).unwrap();
            let succeeded = config::remove(key)
                .map_err(|err| warn!("error on removing config: {:?}", err))
                .is_ok();

            respond!(drtioaux::Payload::CoreMgmtReply { succeeded })
        }
        drtioaux::Payload::CoreMgmtConfigEraseRequest => {
            let succeeded = config::erase()
                .map_err(|err| warn!("error on erasing config: {:?}", err))
                .is_ok();

            respond!(drtioaux::Payload::CoreMgmtReply { succeeded })
        }
        drtioaux::Payload::CoreMgmtRebootRequest => {
            respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true });
            warn!("restarting");
            unsafe { spiflash::reload(); }
        }
        drtioaux::Payload::CoreMgmtFlashRequest { payload_length } => {
            coremgr.allocate_image_buffer(payload_length as usize);
            respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true })
        }
        drtioaux::Payload::CoreMgmtFlashAddDataRequest { last, length, data } => {
            coremgr.add_image_data(&data, length as usize);
            if last {
                respond!(drtioaux::Payload::CoreMgmtDropLink)
            } else {
                respond!(drtioaux::Payload::CoreMgmtReply { succeeded: true })
            }
        }
        drtioaux::Payload::CoreMgmtDropLinkAck => {
            #[cfg(not(has_drtio_eem))]
            unsafe {
                csr::gt_drtio::txenable_write(0);
            }

            #[cfg(has_drtio_eem)]
            unsafe {
                csr::eem_transceiver::txenable_write(0);
            }

            coremgr.flash_image();
            warn!("restarting");
            unsafe { spiflash::reload(); }
        }

        _ => {
            warn!("received unexpected aux packet");
        }
    }
}

fn drtiosat_process_errors() {
    let errors = unsafe { csr::drtiosat::protocol_error_read() };
    if errors & 1 != 0 {
        error!("received packet of an unknown type");
    }
    if errors & 2 != 0 {
        error!("received truncated packet");
    }
    if errors & 4 != 0 {
        let destination = unsafe {
            csr::drtiosat::buffer_space_timeout_dest_read()
        };
        error!("timeout attempting to get buffer space from CRI, destination=0x{:02x}", destination)
    }
    let drtiosat_active = unsafe { csr::cri_con::selected_read() == 0 };
    if drtiosat_active {
        // RTIO errors are handled by ksupport and dma manager
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
    }
    unsafe {
        csr::drtiosat::protocol_error_write(errors);
    }
}


#[cfg(has_rtio_crg)]
fn init_rtio_crg() {
    unsafe {
        csr::rtio_crg::pll_reset_write(0);
    }
    clock::spin_us(150);
    let locked = unsafe { csr::rtio_crg::pll_locked_read() != 0 };
    if !locked {
        error!("RTIO clock failed");
    }
}

#[cfg(not(has_rtio_crg))]
fn init_rtio_crg() { }

fn hardware_tick(ts: &mut u64) {
    let now = clock::get_ms();
    if now > *ts {
        #[cfg(has_grabber)]
        board_artiq::grabber::tick();
        *ts = now + 200;
    }
}

#[cfg(all(has_si5324, rtio_frequency = "125.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings
    = si5324::FrequencySettings {
    n1_hs  : 5,
    nc1_ls : 8,
    n2_hs  : 7,
    n2_ls  : 360,
    n31    : 63,
    n32    : 63,
    bwsel  : 4,
    crystal_as_ckin2: true
};

#[cfg(all(has_si5324, rtio_frequency = "100.0"))]
const SI5324_SETTINGS: si5324::FrequencySettings
    = si5324::FrequencySettings {
    n1_hs  : 5,
    nc1_ls : 10,
    n2_hs  : 10,
    n2_ls  : 250,
    n31    : 50,
    n32    : 50,
    bwsel  : 4,
    crystal_as_ckin2: true
};

#[cfg(all(has_si549, rtio_frequency = "125.0"))]
const SI549_SETTINGS: si549::FrequencySetting = si549::FrequencySetting {
    main: si549::DividerConfig {
        hsdiv: 0x058,
        lsdiv: 0,
        fbdiv: 0x04815791F25,
    },
    helper: si549::DividerConfig {
        // 125MHz*32767/32768
        hsdiv: 0x058,
        lsdiv: 0,
        fbdiv: 0x04814E8F442,
    },
};

#[cfg(all(has_si549, rtio_frequency = "100.0"))]
const SI549_SETTINGS: si549::FrequencySetting = si549::FrequencySetting {
    main: si549::DividerConfig {
        hsdiv: 0x06C,
        lsdiv: 0,
        fbdiv: 0x046C5F49797,
    },
    helper: si549::DividerConfig {
        // 100MHz*32767/32768
        hsdiv: 0x06C,
        lsdiv: 0,
        fbdiv: 0x046C5670BBD,
    },
};

#[cfg(not(soc_platform = "efc"))]
fn sysclk_setup() {
    let switched = unsafe {
        csr::crg::switch_done_read()
    };
    if switched == 1 {
        info!("Clocking has already been set up.");
        return;
    }
    else {
        #[cfg(has_si5324)]
        si5324::setup(&SI5324_SETTINGS, si5324::Input::Ckin1).expect("cannot initialize Si5324");
        #[cfg(has_si549)]
        si549::main_setup(&SI549_SETTINGS).expect("cannot initialize main Si549");

        info!("Switching sys clock, rebooting...");
        // delay for clean UART log, wait until UART FIFO is empty
        clock::spin_us(3000);
        unsafe {
            csr::gt_drtio::stable_clkin_write(1);
        }
        loop {}
    }
}

fn setup_log_levels() {
    match config::read_str("log_level", |r| r.map(|s| s.parse())) {
        Ok(Ok(log_level_filter)) => {
            info!("log level set to {} by `log_level` config key",
                  log_level_filter);
            log::set_max_level(log_level_filter);
        }
        _ => info!("log level set to INFO by default")
    }
    match config::read_str("uart_log_level", |r| r.map(|s| s.parse())) {
        Ok(Ok(uart_log_level_filter)) => {
            info!("UART log level set to {} by `uart_log_level` config key",
                  uart_log_level_filter);
            logger_artiq::BufferLogger::with(|logger|
                logger.set_uart_log_level(uart_log_level_filter));
        }
        _ => info!("UART log level set to INFO by default")
    }
}

static mut LOG_BUFFER: [u8; 1<<17] = [0; 1<<17];

#[no_mangle]
pub extern fn main() -> i32 {
    extern {
        static mut _fheap: u8;
        static mut _eheap: u8;
        static mut _sstack_guard: u8;
    }

    unsafe {
        ALLOC.add_range(&mut _fheap, &mut _eheap);
        pmp::init_stack_guard(&_sstack_guard as *const u8 as usize);
    }
    #[cfg(soc_platform = "kasli")]
    irq::enable_interrupts();
    #[cfg(has_wrpll)]
    irq::enable(csr::WRPLL_INTERRUPT);

    clock::init();
    unsafe {
        logger_artiq::BufferLogger::new(&mut LOG_BUFFER[..]).register(||
            boot::start_user(startup as usize));
    }

    0
}

fn startup() {
    info!("ARTIQ satellite manager starting...");
    info!("software ident {}", csr::CONFIG_IDENTIFIER_STR);
    info!("gateware ident {}", ident::read(&mut [0; 64]));

    setup_log_levels();

    #[cfg(has_i2c)]
    i2c::init().expect("I2C initialization failed");
    #[cfg(all(soc_platform = "kasli", hw_rev = "v2.0"))]
    let (mut io_expander0, mut io_expander1);
    #[cfg(all(soc_platform = "kasli", hw_rev = "v2.0"))]
    {
        io_expander0 = board_misoc::io_expander::IoExpander::new(0).unwrap();
        io_expander1 = board_misoc::io_expander::IoExpander::new(1).unwrap();
        io_expander0.init().expect("I2C I/O expander #0 initialization failed");
        io_expander1.init().expect("I2C I/O expander #1 initialization failed");

        // Actively drive TX_DISABLE to false on SFP0..3
        io_expander0.set_oe(0, 1 << 1).unwrap();
        io_expander0.set_oe(1, 1 << 1).unwrap();
        io_expander1.set_oe(0, 1 << 1).unwrap();
        io_expander1.set_oe(1, 1 << 1).unwrap();
        io_expander0.set(0, 1, false);
        io_expander0.set(1, 1, false);
        io_expander1.set(0, 1, false);
        io_expander1.set(1, 1, false);
        io_expander0.service().unwrap();
        io_expander1.service().unwrap();
    }

    #[cfg(not(soc_platform = "efc"))]
    sysclk_setup();

    #[cfg(has_si549)]
    si549::helper_setup(&SI549_SETTINGS).expect("cannot initialize helper Si549");    

    #[cfg(soc_platform = "efc")]
    let mut io_expander;
    #[cfg(soc_platform = "efc")]
    {
        let p3v3_fmc_en_pin;
        let vadj_fmc_en_pin;

        #[cfg(hw_rev = "v1.0")]
        {
            p3v3_fmc_en_pin = 0;
            vadj_fmc_en_pin = 1;
        }
        #[cfg(hw_rev = "v1.1")]
        {
            p3v3_fmc_en_pin = 1;
            vadj_fmc_en_pin = 7;
        }

        io_expander = board_misoc::io_expander::IoExpander::new().unwrap();
        io_expander.init().expect("I2C I/O expander initialization failed");

        // Enable LEDs
        io_expander.set_oe(0, 1 << 5 | 1 << 6 | 1 << 7).unwrap();
        
        // Enable VADJ and P3V3_FMC
        io_expander.set_oe(1, 1 << p3v3_fmc_en_pin | 1 << vadj_fmc_en_pin).unwrap();

        io_expander.set(1, p3v3_fmc_en_pin, true);
        io_expander.set(1, vadj_fmc_en_pin, true);

        io_expander.service().unwrap();
    }

    #[cfg(not(has_drtio_eem))]
    unsafe {
        csr::gt_drtio::txenable_write(0xffffffffu32 as _);
    }

    #[cfg(has_drtio_eem)]
    unsafe {
        csr::eem_transceiver::txenable_write(0xffffffffu32 as _);
    }

    init_rtio_crg();

    config::read_str("sed_spread_enable", |r| {
        match r {
            Ok("1") => { info!("SED spreading enabled"); toggle_sed_spread(1); },
            Ok("0") => { info!("SED spreading disabled"); toggle_sed_spread(0); },
            Ok(_) => { 
                warn!("sed_spread_enable value not supported (only 1, 0 allowed), disabling by default");
                toggle_sed_spread(0);
            },
            Err(_) => { info!("SED spreading disabled by default"); toggle_sed_spread(0) },
        }
    });

    #[cfg(has_drtio_eem)]
    {
        drtio_eem::init();
        unsafe {
            csr::eem_transceiver::rx_ready_write(1)
        }
    }

    #[cfg(has_drtio_routing)]
    let mut repeaters = [repeater::Repeater::default(); csr::DRTIOREP.len()];
    #[cfg(not(has_drtio_routing))]
    let mut repeaters = [repeater::Repeater::default(); 0];
    for i in 0..repeaters.len() {
        repeaters[i] = repeater::Repeater::new(i as u8);
    }

    let mut hardware_tick_ts = 0;

    let mut aux_mgr = aux::AuxManager::new();

    #[cfg(soc_platform = "efc")]
    ad9117::init().expect("AD9117 initialization failed");
    
    loop {
        while !drtiosat_link_rx_up() {
            drtiosat_process_errors();
            aux_mgr.service(&mut repeaters);
            #[cfg(all(soc_platform = "kasli", hw_rev = "v2.0"))]
            {
                io_expander0.service().expect("I2C I/O expander #0 service failed");
                io_expander1.service().expect("I2C I/O expander #1 service failed");
            }
            #[cfg(soc_platform = "efc")]
            io_expander.service().expect("I2C I/O expander service failed");
            hardware_tick(&mut hardware_tick_ts);
        }

        info!("uplink is up, switching to recovered clock");
        #[cfg(has_si5324)]
        {
            si5324::siphaser::select_recovered_clock(true).expect("failed to switch clocks");
            si5324::siphaser::calibrate_skew().expect("failed to calibrate skew");
        }

        #[cfg(has_wrpll)]
        si549::wrpll::select_recovered_clock(true);

        // various managers created here, so when link is dropped, DMA traces,
        // analyzer logs, kernels are cleared and/or stopped for a clean slate
        // on subsequent connections, without a manual intervention.
        let mut dma_manager = DmaManager::new();
        let mut analyzer = Analyzer::new();
        let mut kernelmgr = KernelManager::new();
        let mut coremgr = CoreManager::new();

        cricon_select(RtioMaster::Drtio);
        drtioaux::reset(0);
        drtiosat_reset(false);
        drtiosat_reset_phy(false);

        while drtiosat_link_rx_up() {
            drtiosat_process_errors();

            aux_mgr.service(&mut repeaters);
            let transaction = aux_mgr.get_incoming_packet();
            if let Some((transaction_id, source, packet)) = transaction {
                process_aux_packet(&mut dma_manager, &mut analyzer,
                    &mut kernelmgr, &mut repeaters, &mut aux_mgr,
                    &mut coremgr, &packet, transaction_id, source);
            }

            #[cfg(all(soc_platform = "kasli", hw_rev = "v2.0"))]
            {
                io_expander0.service().expect("I2C I/O expander #0 service failed");
                io_expander1.service().expect("I2C I/O expander #1 service failed");
            }
            #[cfg(soc_platform = "efc")]
            io_expander.service().expect("I2C I/O expander service failed");
            hardware_tick(&mut hardware_tick_ts);
            
            if let Some(status) = dma_manager.get_status() {
                info!("playback done, error: {}, channel: {}, timestamp: {}", status.error, status.channel, status.timestamp);
                aux_mgr.transact(status.source, drtioaux::Payload::DmaPlaybackStatus { 
                    id: status.id, error: status.error, channel: status.channel, timestamp: status.timestamp 
                }).unwrap();
            }

            kernelmgr.process_kern_requests(&mut aux_mgr, &mut dma_manager);
        }

        drtiosat_reset_phy(true);
        drtiosat_reset(true);
        drtiosat_tsc_loaded();
        info!("uplink is down, switching to local oscillator clock");
        #[cfg(has_si5324)]
        si5324::siphaser::select_recovered_clock(false).expect("failed to switch clocks");
        #[cfg(has_wrpll)]
        si549::wrpll::select_recovered_clock(false);
    }
}

#[cfg(soc_platform = "efc")]
fn enable_error_led() {
    let p3v3_fmc_en_pin;
    let vadj_fmc_en_pin;

    #[cfg(hw_rev = "v1.0")]
    {
        p3v3_fmc_en_pin = 0;
        vadj_fmc_en_pin = 1;
    }
    #[cfg(hw_rev = "v1.1")]
    {
        p3v3_fmc_en_pin = 1;
        vadj_fmc_en_pin = 7;
    }

    let mut io_expander = board_misoc::io_expander::IoExpander::new().unwrap();

    // Keep LEDs enabled
    io_expander.set_oe(0, 1 << 5 | 1 << 6 | 1 << 7).unwrap();
    // Enable Error LED
    io_expander.set(0, 7, true);

    // Keep VADJ and P3V3_FMC enabled
    io_expander.set_oe(1, 1 << p3v3_fmc_en_pin | 1 << vadj_fmc_en_pin).unwrap();

    io_expander.set(1, p3v3_fmc_en_pin, true);
    io_expander.set(1, vadj_fmc_en_pin, true);

    io_expander.service().unwrap();
}

#[no_mangle]
pub extern fn exception(_regs: *const u32) {
    let pc = mepc::read();
    let cause = mcause::read().cause();
    match cause {
        mcause::Trap::Interrupt(_source) => {
            #[cfg(has_wrpll)]
            if irq::is_pending(csr::WRPLL_INTERRUPT) {
                si549::wrpll::interrupt_handler();
            }
        },

        mcause::Trap::Exception(e) => {
            fn hexdump(addr: u32) {
                let addr = (addr - addr % 4) as *const u32;
                let mut ptr  = addr;
                println!("@ {:08p}", ptr);
                for _ in 0..4 {
                    print!("+{:04x}: ", ptr as usize - addr as usize);
                    print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
                    print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
                    print!("{:08x} ",   unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
                    print!("{:08x}\n",  unsafe { *ptr }); ptr = ptr.wrapping_offset(1);
                }
            }

            hexdump(u32::try_from(pc).unwrap());
            let mtval = mtval::read();
            panic!("exception {:?} at PC 0x{:x}, trap value 0x{:x}", e, u32::try_from(pc).unwrap(), mtval)
        }
    }
}

#[no_mangle]
pub extern fn abort() {
    println!("aborted");
    loop {}
}

#[no_mangle] // https://github.com/rust-lang/rust/issues/{38281,51647}
#[panic_handler]
pub fn panic_fmt(info: &core::panic::PanicInfo) -> ! {
    #[cfg(has_error_led)]
    unsafe {
        csr::error_led::out_write(1);
    }

    if let Some(location) = info.location() {
        print!("panic at {}:{}:{}", location.file(), location.line(), location.column());
        #[cfg(soc_platform = "efc")]
        {
            if location.file() != "libboard_misoc/io_expander.rs" {
                enable_error_led();
            }
        }
    } else {
        print!("panic at unknown location");
        #[cfg(soc_platform = "efc")]
        enable_error_led();
    }
    if let Some(message) = info.message() {
        println!(": {}", message);
    } else {
        println!("");
    }
    loop {}
}
