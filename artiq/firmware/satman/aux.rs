use alloc::{boxed::Box, collections::BTreeMap, vec::Vec};
use core::{cmp::min, cell::RefCell};
use board_misoc::{csr, clock};
use board_artiq::{drtioaux, drtio_routing};
use proto_artiq::drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, SAT_PAYLOAD_MAX_SIZE, PayloadStatus};

use repeater;
use drtiosat_tsc_loaded;
use drtiosat_link_rx_up;
use drtiosat_reset;

type TransactionHandle = u8;

pub const LINK_COOLDOWN: u64 = 5;
pub const DEFAULT_TIMEOUT: u64 = 200;

/* represents large data that has to be sent with the aux protocol */
#[derive(Debug)]
pub struct Sliceable {
    it: usize,
    data: Vec<u8>,
    destination: u8
}

pub struct SliceMeta {
    pub destination: u8,
    pub len: u16,
    pub status: PayloadStatus
}

macro_rules! get_slice_fn {
    ( $name:tt, $size:expr ) => {
        pub fn $name(&mut self, data_slice: &mut [u8; $size]) -> SliceMeta {
            let first = self.it == 0;
            let len = min($size, self.data.len() - self.it);
            let last = self.it + len == self.data.len();
            let status = PayloadStatus::from_status(first, last);
            data_slice[..len].clone_from_slice(&self.data[self.it..self.it+len]);
            self.it += len;
    
            SliceMeta {
                destination: self.destination,
                len: len as u16,
                status: status
            }
        }
    };
}

impl Sliceable {
    pub fn new(destination: u8, data: Vec<u8>) -> Sliceable {
        Sliceable {
            it: 0,
            data: data,
            destination: destination
        }
    }

    pub fn at_end(&self) -> bool {
        self.it == self.data.len()
    }

    pub fn extend(&mut self, data: &[u8]) {
        self.data.extend(data);
    }

    get_slice_fn!(get_slice_sat, SAT_PAYLOAD_MAX_SIZE);
    get_slice_fn!(get_slice_master, MASTER_PAYLOAD_MAX_SIZE);
}

#[derive(Debug)]
pub enum Error {
    TransactionLimitReached,
    Timeout,
    AuxError(drtioaux::Error<!>)
}

impl From<drtioaux::Error<!>> for Error {
    fn from(value: drtioaux::Error<!>) -> Error {
        Error::AuxError(value)
    }
}

/* represents packets that arrive to this device */
#[derive(PartialEq)]
enum IncomingTransactionState {
    Received,
    Handled
}

struct IncomingTransaction {
    pub payload: drtioaux::Payload,
    pub state: IncomingTransactionState,
}

impl IncomingTransaction {
    fn new(payload: drtioaux::Payload) -> IncomingTransaction {
        IncomingTransaction {
            payload: payload,
            state: IncomingTransactionState::Received
        }
    }
}

#[derive(PartialEq, Debug)]
enum TransactionState {
    Unsent,
    Sent,
    Received(drtioaux::Payload),
    TimedOut,
    BeenRead,
}

/* represents transactions started by this device */
struct OutgoingTransaction {
    packet: drtioaux::Packet,
    max_time: u64,
    state: TransactionState,
    force_linkno: Option<u8>, // linkno not determined by routing table (reset packets need that)
}

impl OutgoingTransaction {
    pub fn new(packet: drtioaux::Packet, force_linkno: Option<u8>) -> OutgoingTransaction {
        OutgoingTransaction {
            packet: packet,
            max_time: clock::get_ms() + DEFAULT_TIMEOUT,
            state: TransactionState::Unsent,
            force_linkno: force_linkno,
        }
    }

    pub fn check_state(&mut self) -> Result<Option<drtioaux::Payload>, Error> {
        // called by subkernel handler code
        match self.state {
            TransactionState::Received(response) => { self.state = TransactionState::BeenRead; Ok(Some(response)) },
            TransactionState::TimedOut => { self.state = TransactionState::BeenRead; Err(Error::Timeout)},
            _ => Ok(None)
        }
    }

    pub fn record_response(&mut self, response: &drtioaux::Payload) {
        self.state = TransactionState::Received(*response);
    }

    pub fn can_be_deleted(&self) -> bool {
        self.state == TransactionState::BeenRead
    }

    pub fn should_send(&mut self, current_time: u64) -> bool {
        // returns true if message needs to be sent
        // checks for timeout first
        if (self.state == TransactionState::Unsent ||
                self.state == TransactionState::Sent) &&
                current_time > self.max_time {
            self.state = TransactionState::TimedOut;
            false
        } else {
            match self.state {
                TransactionState::Unsent => true,
                _ => false
            }
        }
    }

    pub fn update(&mut self) {
        // state updated only after successful send
        if self.state == TransactionState::Unsent {
            self.state = TransactionState::Sent;
        }
    }
}

#[derive(PartialEq)]
enum UpstreamState {
    Down,
    SettingUp,
    Up
}

pub struct AuxManager {
    incoming_transactions: BTreeMap<(TransactionHandle, u8), IncomingTransaction>,
    outgoing_transactions: [Option<Box<OutgoingTransaction>>; 128],
    routable_packets: Vec<drtioaux::Packet>,
    next_id: TransactionHandle,
    self_destination: u8,
    rank: u8,
    routing_table: RefCell<drtio_routing::RoutingTable>,
    upstream_state: UpstreamState,
}

impl AuxManager {
    pub fn new() -> AuxManager {
        AuxManager {
            incoming_transactions: BTreeMap::new(),
            outgoing_transactions: [None; 128],
            routable_packets: Vec::new(),
            next_id: 0,
            self_destination: 1,
            rank: 1,
            routing_table: RefCell::new(drtio_routing::RoutingTable::default_empty()),
            upstream_state: UpstreamState::Down
        }
    }

    pub fn transact(&mut self, destination: u8, payload: drtioaux::Payload
    ) -> Result<TransactionHandle, Error> {
        let mut i = 0;
        self.next_id = (self.next_id + 1) % 128;
        while self.outgoing_transactions[self.next_id as usize].is_some() && i < 128 {
            self.next_id = (self.next_id + 1) % 128;
            i += 1;
        }
        if i == 128 {
            return Err(Error::TransactionLimitReached);
        }
        let transaction_id = self.next_id;
        let transaction = Box::new(OutgoingTransaction::new(
            drtioaux::Packet { 
                source: self.self_destination,
                destination: destination,
                transaction_id: transaction_id,
                payload: payload
            }, None));
        // will be dealt with by the send thread
        self.outgoing_transactions[transaction_id as usize] = Some(transaction);
        Ok(transaction_id)
    }

    pub fn check_transaction(&mut self, transaction_id: TransactionHandle) -> Result<Option<drtioaux::Payload>, Error> {
        self.outgoing_transactions[transaction_id as usize].as_mut().unwrap().check_state()
    }

    pub fn get_destination(&self, transaction_id: TransactionHandle) -> u8 {
        self.outgoing_transactions[transaction_id as usize].as_ref().unwrap().packet.destination
    }

    pub fn service(&mut self, repeaters: &mut [repeater::Repeater]) {
        for rep in repeaters.iter_mut() {
            let packet = rep.service(&self.routing_table.borrow(), self.rank);
            if let Ok(Some(packet)) = packet {
                self.route_packet(&packet);
            }
        }
        if !drtiosat_link_rx_up() {
            self.upstream_state = UpstreamState::Down;
        } else {
            if self.upstream_state == UpstreamState::Down {
                self.upstream_state = UpstreamState::SettingUp;
            }
            if drtiosat_tsc_loaded() {
                info!("TSC loaded from uplink");
                for rep in repeaters.iter() {
                    if let Err(e) = rep.sync_tsc() {
                        error!("failed to sync TSC ({})", e);
                    }
                }
                if let Err(e) = drtioaux::send(0, &drtioaux::Packet {
                    source: 0, destination: 0, transaction_id: 0,
                    payload: drtioaux::Payload::TSCAck 
                }) {
                    error!("aux packet error: {}", e);
                }
            }
            // receive packets from upstream
            let upstream_recv = drtioaux::recv(0);
            if let Err(e) = upstream_recv {
                error!("error receiving packet from upstream: {:?}", e);
            } else if let Some(packet) = upstream_recv.unwrap() {
                if !self.handle_setup_packet(&packet, repeaters) {
                    if self.upstream_state != UpstreamState::SettingUp {
                        self.route_packet(&packet);
                    }
                }
            }
        }
        // deal with sending and incoming transactions
        let current_time = clock::get_ms();
        // satisfy borrow checker by extracting fields we need for sending
        let routing_table = self.routing_table.borrow();
        let rank = self.rank;
        let upstream_state = &mut self.upstream_state;
    
        self.routable_packets.retain(|packet| {
            match send(repeaters, current_time, None, 
                &routing_table, rank, upstream_state, 
                packet) {
                Ok(value) => !value,
                // repeater errors (link down) end in discarding the packet
                Err(e) => { error!("error sending routable packet: {:?}", e); false }
            }
        });
        for entry in self.outgoing_transactions.iter_mut() {
            if let Some(transaction) = entry {
                if transaction.should_send(current_time) {
                    match send(repeaters, current_time, transaction.force_linkno, 
                        &routing_table, rank, upstream_state, 
                        &transaction.packet) {
                        Ok(true) => transaction.update(),
                        Ok(false) => (),
                        Err(e) => error!("error sending outgoing transaction: {:?}", e)
                    };
                    break;
                } else if transaction.can_be_deleted() {
                    *entry = None;
                }
            }   
        }
        self.incoming_transactions.retain(|_, transaction| {
            transaction.state != IncomingTransactionState::Handled
        });

    }

    fn route_packet(&mut self, packet: &drtioaux::Packet) {
        // route the packet either to local transaction or up/downstream
        if packet.destination == self.self_destination {
            if packet.transaction_id & 0x80 != 0 {
                // acknowledge responses
                let transaction = self.outgoing_transactions[packet.transaction_id as usize].as_mut();
                if let Some(local_transaction) = transaction {
                    local_transaction.record_response(&packet.payload);
                } else {
                    error!("received PacketAck for non-existing transaction")
                }
            } else {
                // incoming transactions and responses to local outgoing
                let transaction = self.outgoing_transactions[(packet.transaction_id & 0x7F) as usize].as_mut();
                let is_expected = packet.transaction_id & 0x80 != 0 && match transaction {
                        Some(ref transaction) => transaction.packet.destination == packet.source,
                        _ => false
                    };
                if is_expected {
                    transaction.unwrap().record_response(&packet.payload);
                } else {
                    self.incoming_transactions.insert((packet.transaction_id, packet.source), IncomingTransaction::new(packet.payload));
                }
            }

        } else {
            #[cfg(has_drtio_routing)]
            self.routable_packets.push(packet.clone());
            #[cfg(not(has_drtio_routing))]
            error!("received packet to be routed without routing support: {:?}", packet);
        }
    }

    fn handle_setup_packet(&mut self, packet: &drtioaux::Packet, repeaters: &mut [repeater::Repeater]) -> bool {
        // returns true if packet was consumed
        match packet.payload {
            drtioaux::Payload::EchoRequest => {
                drtioaux::send(0, &drtioaux::Packet { 
                    source: 0,
                    destination: 0,
                    transaction_id: 0,
                    payload: drtioaux::Payload::EchoReply
                }).unwrap();
                true
            }
            drtioaux::Payload::RoutingSetPath { destination: _destination, hops: _hops } => {
                #[cfg(has_drtio_routing)]
                {
                    let mut routing_table = self.routing_table.borrow_mut();
                    routing_table.0[_destination as usize] = _hops;
                    for rep in repeaters.iter() {
                        if let Err(e) = rep.set_path(_destination, &_hops) {
                            error!("failed to set path ({})", e);
                        }
                    }
                }
                drtioaux::send(0, &drtioaux::Packet { 
                    source: 0,
                    destination: 0,
                    transaction_id: 0,
                    payload: drtioaux::Payload::RoutingAck
                }).unwrap();
                true
            }
            drtioaux::Payload::RoutingSetRank { rank } => {
                self.rank = rank;
                #[cfg(has_drtio_routing)]
                {
                    drtio_routing::interconnect_enable_all(&self.routing_table.borrow(), rank);
                    let rep_rank = rank + 1;
                    for rep in repeaters.iter() {
                        if let Err(e) = rep.set_rank(rep_rank) {
                            error!("failed to set rank ({})", e);
                        }
                    }
                    info!("rank: {}", rank);
                    info!("routing table: {}", self.routing_table.borrow());
                }
                drtioaux::send(0, &drtioaux::Packet { 
                    source: 0,
                    destination: 0,
                    transaction_id: 0,
                    payload: drtioaux::Payload::RoutingAck
                }).unwrap();
                self.upstream_state = UpstreamState::Up;
                true
            }
            // DestinationStatusRequest will come only from the master, so it is not handled in route_packet
            drtioaux::Payload::DestinationStatusRequest => {
                #[cfg(has_drtio_routing)]
                if packet.destination != self.self_destination {
                    let repno = self.routing_table.borrow().0[packet.destination as usize][self.rank as usize] as usize - 1;
                    if !repeaters[repno].is_up() {
                        self.respond(packet.source, packet.transaction_id, &drtioaux::Payload::DestinationDownReply);
                        return true;
                    }
                }
                false
            }
            drtioaux::Payload::ResetRequest => {
                info!("resetting RTIO");
                drtiosat_reset(true);
                clock::spin_us(100);
                drtiosat_reset(false);
                for rep in repeaters.iter() {
                    if let Err(e) = rep.rtio_reset() {
                        error!("failed to issue RTIO reset ({})", e);
                    }
                }
                drtioaux::send(0, &drtioaux::Packet { 
                    source: 0,
                    destination: 0,
                    transaction_id: packet.transaction_id | 0x80,
                    payload: drtioaux::Payload::ResetAck
                }).unwrap();
                false
            }
            // packet is not consumed, returned
            _ => false
        }
    }

    pub fn respond(&mut self, transaction_id: u8, source: u8, response: &drtioaux::Payload) {
        let transaction_id = transaction_id | 0x80;
        self.routable_packets.push(drtioaux::Packet { 
            source: self.self_destination,
            destination: source,
            transaction_id: transaction_id,
            payload: *response
        });
    }

    pub fn get_incoming_packet(&mut self) -> Option<(u8, u8, drtioaux::Payload)> {
        for ((transaction_id, source), transaction) in self.incoming_transactions.iter_mut() {
            if transaction.state == IncomingTransactionState::Received {
                transaction.state = IncomingTransactionState::Handled;
                return Some((*transaction_id, *source, transaction.payload))
            }
        }
        None
    }

    pub fn self_destination(&self) -> u8 {
        self.self_destination
    }
}

fn send(_repeaters: &mut [repeater::Repeater], current_time: u64, _force_linkno: Option<u8>,
    routing_table: &drtio_routing::RoutingTable, rank: u8, upstream_state: &UpstreamState,
    packet: &drtioaux::Packet,
) -> Result<bool, drtioaux::Error<!>> {
    #[cfg(has_drtio_routing)]
    {
        let hop = _force_linkno.unwrap_or(routing_table.0[packet.destination as usize][rank as usize]) as usize;
        if hop > 0 && hop < csr::DRTIOREP.len() {
            let repno = (hop - 1) as usize;
            return _repeaters[repno].aux_send(current_time, packet);
        }
    }
    if let UpstreamState::Up = *upstream_state {
        drtioaux::send(0, packet)?;
        return Ok(true);
    }
    Ok(false)
}