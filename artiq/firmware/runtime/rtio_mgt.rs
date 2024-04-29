use core::cell::RefCell;
use urc::Urc;
use board_misoc::{csr, config};
#[cfg(has_drtio)]
use board_misoc::clock;
use board_artiq::drtio_routing;
use sched::Io;
use sched::Mutex;
use io::{Cursor, ProtoRead};
use session_proto::{DeviceMap, resolve_channel_name, set_device_map};
const ASYNC_ERROR_COLLISION: u8 = 1 << 0;
const ASYNC_ERROR_BUSY: u8 = 1 << 1;
const ASYNC_ERROR_SEQUENCE_ERROR: u8 = 1 << 2;

#[cfg(has_drtio)]
pub mod drtio {
    use super::*;
    use alloc::{vec::Vec, boxed::Box};
    use drtioaux;
    use proto_artiq::drtioaux_proto::{MASTER_PAYLOAD_MAX_SIZE, PayloadStatus};
    use rtio_dma::remote_dma;
    use kernel::subkernel;
    use sched::{Error as SchedError, BinarySemaphore};

    #[derive(Fail, Debug)]
    pub enum Error {
        #[fail(display = "timed out")]
        Timeout,
        #[fail(display = "unexpected packet: {:?}", _0)]
        UnexpectedPacket(drtioaux::Payload),
        #[fail(display = "aux packet error: {:?}", _0)]
        AuxError(drtioaux::Error<!>),
        #[fail(display = "link down")]
        LinkDown,
        #[fail(display = "unexpected reply")]
        UnexpectedReply,
        #[fail(display = "sched error: {}", _0)]
        SchedError(#[cause] SchedError),
        #[fail(display = "transaction does not exist")]
        TransactionDoesNotExist,
        #[fail(display = "transaction in wrong state")]
        TransactionWrongState,
        #[fail(display = "open transaction limit reached")]
        TransactionLimitReached,
        #[fail(display = "transaction failed")]
        TransactionFailed,
    }

    impl From<SchedError> for Error {
        fn from(value: SchedError) -> Error {
            Error::SchedError(value)
        }
    }

    impl From<drtioaux::Error<!>> for Error {
        fn from(error: drtioaux::Error<!>) -> Error {
            Error::AuxError(error)
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

    type TransactionHandle = u8;

    pub const DEFAULT_TIMEOUT: u64 = 200;

    struct Transaction {
        /* Packet to be sent */
        packet: drtioaux::Packet,
        /* last time of sending/resending */
        last_action_time: u64,
        /* maximum time for transaction before timeout */
        max_time: u64,
        /* transaction state, must be a RefCell to work with borrow checker */
        state: RefCell<TransactionState>,
        /* semaphore to wait on for other threads for transaction to end */
        semaphore: BinarySemaphore,
        /* when linkno is not determined by routing table by destination (reset packets need that) */
        force_linkno: Option<u8>,
    }

    impl Transaction {
        pub fn new(packet: drtioaux::Packet, force_linkno: Option<u8>) -> Transaction {
            let last_action_time = clock::get_ms();
            Transaction {
                packet: packet,
                last_action_time: last_action_time,
                max_time: last_action_time + DEFAULT_TIMEOUT,
                state: RefCell::new(TransactionState::Unsent),
                semaphore: BinarySemaphore::new(false),
                force_linkno: force_linkno,
            }
        }

        pub fn wait(&mut self, io: &Io) -> Result<drtioaux::Payload, Error> {
            let need_wait = match *self.state.borrow() {
                TransactionState::Unsent |
                    TransactionState::Sent => true,
                _ => false
            };
            if need_wait {
                self.semaphore.wait(io)?;
            }
            let mut state = self.state.borrow_mut();
            let ret = match *state {
                TransactionState::Received(response) => Ok(response),
                TransactionState::TimedOut => Err(Error::Timeout),
                _ => {
                    error!("wrong state: {:?}", *state);
                    Err(Error::TransactionWrongState)
                }
            };
            *state = TransactionState::BeenRead;
            ret
        }

        pub fn record_response(&mut self, response: drtioaux::Payload) {
            let mut state = self.state.borrow_mut();
            if *state == TransactionState::Sent {
                *state = TransactionState::Received(response);
                self.semaphore.signal();
            }
        }

        pub fn can_be_deleted(&self, current_time: u64) -> bool {
            match *self.state.borrow() {
                TransactionState::BeenRead => true,
                TransactionState::TimedOut => current_time > self.last_action_time + DEFAULT_TIMEOUT,
                _ => false
            }
        }

        pub fn should_send(&mut self, current_time: u64) -> bool {
            // returns true if message needs to be sent
            // checks for timeout first
            let mut state = self.state.borrow_mut();
            if (*state == TransactionState::Unsent ||
                    *state == TransactionState::Sent) &&
                    current_time > self.max_time {
                *state = TransactionState::TimedOut;
                self.semaphore.signal();
                false
            } else {
                match *state {
                    TransactionState::Unsent => true,
                    TransactionState::Sent => current_time >= self.last_action_time + DEFAULT_TIMEOUT,
                    _ => false
                }    
            }
        }

        pub fn update_last_action_time(&mut self, current_time: u64) {
            // state updated only after successful send
            let mut state = self.state.borrow_mut();
            if *state == TransactionState::Unsent {
                *state = TransactionState::Sent;
                self.last_action_time = current_time;
            } else if *state == TransactionState::Sent {
                self.last_action_time = current_time;
            }
        }
    }

    struct TransactionManager {
        transactions: [Option<Box<Transaction>>; 128],
        routable_packets: Vec<drtioaux::Packet>,
        next_id: TransactionHandle,
        recv_flush: Option<(u64, BinarySemaphore)>,
        self_destination: u8,
    }

    impl TransactionManager {
        pub const fn new() -> TransactionManager {
            TransactionManager {
                transactions: [None; 128],
                routable_packets: Vec::new(),
                next_id: 0,
                recv_flush: None,
                self_destination: 0,
            }
        }

        pub fn set_self_destination(&mut self, dest: u8) {
            self.self_destination = dest;
        }

        pub fn transact(&mut self, io: &Io, destination: u8, payload: &drtioaux::Payload, force_linkno: Option<u8>
        ) -> Result<drtioaux::Payload, Error> {
            let handle = self.transact_async(destination, payload, force_linkno)?;
            self.transactions[handle as usize].as_mut().unwrap().wait(io)
        }

        pub fn transact_async(&mut self, destination: u8, payload: &drtioaux::Payload, force_linkno: Option<u8>
        ) -> Result<TransactionHandle, Error> {
            let mut i = 0;
            self.next_id = (self.next_id + 1) % 128;
            while self.transactions[self.next_id as usize].is_some() && i < 128 {
                self.next_id = (self.next_id + 1) % 128;
                i += 1;
            }
            if i == 128 {
                return Err(Error::TransactionLimitReached);
            }
            let transaction_id = self.next_id;
            let transaction = Box::new(Transaction::new(
                drtioaux::Packet { 
                    source: self.self_destination,
                    destination: destination,
                    transaction_id: transaction_id,
                    payload: *payload
                }, force_linkno));
            // will be dealt with by the send thread
            self.transactions[transaction_id as usize] = Some(transaction);
            Ok(transaction_id)
        }

        pub fn await_transaction(&mut self, io: &Io, handle: TransactionHandle) -> Result<drtioaux::Payload, Error> {
            match self.transactions[handle as usize].as_mut() {
                Some(transaction) => transaction.wait(io),
                None => Err(Error::TransactionDoesNotExist)
            }
        }

        pub fn handle_response(&mut self, io: &Io, ddma_mutex: &Mutex, 
            subkernel_mutex: &Mutex, packet: &drtioaux::Packet) {
            let transaction = self.transactions[(packet.transaction_id & 0x7F) as usize].as_mut();
            let is_expected = packet.transaction_id & 0x80 != 0 && match transaction {
                Some(ref transaction) => {
                    transaction.packet.destination == packet.source
                }
                _ => false
            };
            if is_expected {
                transaction.unwrap().record_response(packet.payload);
            } else {
                match &packet.payload {
                    drtioaux::Payload::DmaPlaybackStatus { id, error, channel, timestamp } => {
                        remote_dma::playback_done(io, ddma_mutex, *id, packet.source, *error, *channel, *timestamp);
                    },
                    drtioaux::Payload::SubkernelFinished { id, with_exception, exception_src } => {
                        subkernel::subkernel_finished(io, subkernel_mutex, *id, *with_exception, *exception_src);
                    },
                    drtioaux::Payload::SubkernelMessage { id, status, length, data } => {
                        subkernel::message_handle_incoming(io, subkernel_mutex, *id, *status, *length as usize, &data);
                        // no subkernelmsgack, normal ack is enough
                    },
                    packet => warn!("received unsolicited packet: {:?}", packet)
                };
            }
        }

        pub fn route_packet(&mut self, packet: &drtioaux::Packet) {
            self.routable_packets.push(packet.clone());
        }

        pub fn send_routable_packet(&mut self, routing_table: &drtio_routing::RoutingTable, 
            link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>) {
            self.routable_packets.retain(|packet| {
                match send(routing_table, link_states, None, packet) {
                    Ok(()) => false,
                    // routable packet is also discarded if link is down
                    Err(Error::LinkDown) => false,
                    Err(e) => { warn!("error rerouting packet: {:?}", e); true }
                }
            });
        }

        pub fn handle_transactions(&mut self, 
            routing_table: &drtio_routing::RoutingTable,
            link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>,
            current_time: u64) {
            for entry in self.transactions.iter_mut() {
                if let Some(transaction) = entry {
                    let should_send = transaction.should_send(current_time);
                    if should_send {
                        match send(routing_table, link_states, transaction.force_linkno, &transaction.packet) {
                            Ok(()) => transaction.update_last_action_time(current_time),
                            Err(Error::LinkDown) => (),
                            Err(e) => warn!("error sending packet: {:?}", e)
                        }
                    } else if transaction.can_be_deleted(current_time) {
                        // clean up finished transactions to free up IDs
                        *entry = None;
                    }
                }
            }
        }

        pub fn recv_flush_check(&mut self) -> bool {
            if let Some((start_time, semaphore)) = &self.recv_flush {
                if clock::get_ms() > start_time + DEFAULT_TIMEOUT {
                    semaphore.signal();
                    self.recv_flush = None;
                } else {
                    return true;
                }
            }
            false
        }

        pub fn recv_flush(&mut self, io: &Io) -> Result<(), Error>{
            // received packets will be discarded for the next 200ms to ensure clean state
            let semaphore = BinarySemaphore::new(false);
            self.recv_flush = Some((clock::get_ms(), semaphore.clone()));
            // hang on a semaphore to prevent any further actions or new transactions
            semaphore.wait(io)?;
            Ok(())
        }
    }

    static mut TRANSACTION_MANAGER: TransactionManager = TransactionManager::new();

    pub fn startup(io: &Io, routing_table: &Urc<RefCell<drtio_routing::RoutingTable>>,
            up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
            ddma_mutex: &Mutex, subkernel_mutex: &Mutex) {
        let link_states = Urc::new(RefCell::new(
            [false; csr::DRTIO.len()]));
        {
            let self_destination = routing_table.borrow().determine_self_destination();
            unsafe { TRANSACTION_MANAGER.set_self_destination(self_destination) }
            let routing_table = routing_table.clone();
            let up_destinations = up_destinations.clone();
            let link_states = link_states.clone();
            let ddma_mutex = ddma_mutex.clone();
            let subkernel_mutex = subkernel_mutex.clone();
            io.spawn(16384*2, move |io| {
                let routing_table = routing_table.borrow();
                link_thread(io, &routing_table, &link_states, &up_destinations, &ddma_mutex, &subkernel_mutex);
            });
        }
        {
            let link_states = link_states.clone();
            let ddma_mutex = ddma_mutex.clone();
            let subkernel_mutex = subkernel_mutex.clone();
            io.spawn(8192, move |io| {
                recv_thread(io, &ddma_mutex, &subkernel_mutex, &link_states);
            });
        }
        {
            let routing_table = routing_table.clone();
            let link_states = link_states.clone();
            io.spawn(16384, move |io| {
                let routing_table = routing_table.borrow();
                send_thread(io, &routing_table, &link_states);
            });
        }
    }

    fn recv_thread(io: Io, ddma_mutex: &Mutex, subkernel_mutex: &Mutex, link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>) {
        loop {
            for linkno in 0..csr::DRTIO.len() {
                if !link_states.borrow()[linkno] {
                    continue;
                }
                let res = drtioaux::recv(linkno as u8);
                if unsafe { TRANSACTION_MANAGER.recv_flush_check() } {
                    continue;
                }
                if let Err(e) = res {
                    warn!("[LINK#{}] aux packet error: {:?}", linkno, e);
                } else if let Ok(Some(packet)) = res {
                    let destination = packet.destination;
                    if destination != 0 {
                        unsafe { TRANSACTION_MANAGER.route_packet(&packet); }
                    } else {
                        unsafe {
                            TRANSACTION_MANAGER.handle_response(&io, ddma_mutex, subkernel_mutex, &packet);
                        }
                    }
                }
            }
            io.relinquish().unwrap();
        }
    }

    fn send(routing_table: &drtio_routing::RoutingTable, 
            link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>, 
            force_linkno: Option<u8>, packet: &drtioaux::Packet) -> Result<(), Error> {
        let linkno = force_linkno.unwrap_or(routing_table.0[packet.destination as usize][0] - 1);
        let link_states = link_states.borrow_mut();
        if linkno as usize > link_states.len() {
            return Err(Error::TransactionFailed);
        }
        if link_states[linkno as usize] {
            drtioaux::send(linkno, packet)?;
            Ok(())
        } else {
            Err(Error::LinkDown)
        }
    }

    fn send_thread(io: Io, routing_table: &drtio_routing::RoutingTable, link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>) {
        loop {
            io.relinquish().unwrap();
            let current_time = clock::get_ms();
            unsafe {
                // reroute packets
                TRANSACTION_MANAGER.send_routable_packet(routing_table, link_states);
                // outgoing transactions
                TRANSACTION_MANAGER.handle_transactions(routing_table, link_states, current_time);
            }
        }
    }

    pub fn aux_transact(io: &Io, destination: u8, payload: &drtioaux::Payload) -> Result<drtioaux::Payload, Error> {
        unsafe { TRANSACTION_MANAGER.transact(io, destination, payload, None) }
    }

    pub fn async_aux_transact(destination: u8, payload: &drtioaux::Payload) -> TransactionHandle {
        unsafe { TRANSACTION_MANAGER.transact_async(destination, payload, None).unwrap() }
    }

    pub fn await_transaction(io: &Io, handle: TransactionHandle) -> Result<drtioaux::Payload, Error> {
        unsafe { TRANSACTION_MANAGER.await_transaction(io, handle) }
    }

    fn link_rx_up(linkno: u8) -> bool {
        let linkno = linkno as usize;
        unsafe {
            (csr::DRTIO[linkno].rx_up_read)() == 1
        }
    }

    pub fn clear_buffers(io: &Io) -> Result<(), Error> {
        unsafe { TRANSACTION_MANAGER.recv_flush(io) }
    }

    fn recv_aux_timeout(io: &Io, linkno: u8, timeout: u32) -> Result<drtioaux::Packet, Error> {
        let max_time = clock::get_ms() + timeout as u64;
        loop {
            if !link_rx_up(linkno) {
                return Err(Error::LinkDown);
            }
            if clock::get_ms() > max_time {
                return Err(Error::Timeout);
            }
            match drtioaux::recv(linkno)? {
                Some(packet) => {
                    return Ok(packet)
                },
                None => (),
            }
            io.relinquish()?;
        }
    }

    fn setup_transact(io: &Io, linkno: u8, payload: &drtioaux::Payload) -> Result<drtioaux::Payload, Error> {
        drtioaux::send(linkno, &drtioaux::Packet {
            source: 0,
            destination: 0,
            transaction_id: 0,
            payload: *payload }).unwrap();
        let reply = recv_aux_timeout(io, linkno, 200)?;
        Ok(reply.payload)
    }

    fn ping_remote(io: &Io, linkno: u8) -> u32 {
        let mut count = 0;
        loop {
            if !link_rx_up(linkno) {
                return 0
            }
            count += 1;
            if count > 100 {
                return 0;
            }
            let reply = setup_transact(io, linkno, &drtioaux::Payload::EchoRequest);
            match reply {
                Ok(drtioaux::Payload::EchoReply) => {
                    // make sure receive buffer is drained
                    let max_time = clock::get_ms() + 200;
                    loop {
                        if clock::get_ms() > max_time {
                            return count;
                        }
                        let _ = drtioaux::recv(linkno);
                        io.relinquish().unwrap();
                    }
                }
                _ => {}
            }
            io.relinquish().unwrap();
        }
    }

    fn sync_tsc(io: &Io, linkno: u8) -> Result<(), Error> {
        unsafe {
            (csr::DRTIO[linkno as usize].set_time_write)(1);
            while (csr::DRTIO[linkno as usize].set_time_read)() == 1 {}
        }
        // TSCAck is the only aux packet that is sent spontaneously
        // by the satellite, in response to a TSC set on the RT link.
        let reply = recv_aux_timeout(io, linkno, 10000)?.payload;
        if reply == drtioaux::Payload::TSCAck {
            return Ok(());
        } else {
            return Err(Error::UnexpectedReply);
        }
    }

    fn load_routing_table(io: &Io,
        linkno: u8, routing_table: &drtio_routing::RoutingTable) -> Result<(), Error> {
        for i in 0..drtio_routing::DEST_COUNT {
            let reply = setup_transact(io, linkno, &drtioaux::Payload::RoutingSetPath {
                destination: i as u8,
                hops: routing_table.0[i]
            })?;
            if reply != drtioaux::Payload::RoutingAck {
                return Err(Error::UnexpectedReply);
            }
        }
        Ok(())
    }

    fn set_rank(io: &Io, linkno: u8, rank: u8) -> Result<(), Error> {
        let reply = setup_transact(io, linkno,
            &drtioaux::Payload::RoutingSetRank {
                rank: rank
            })?;
        if reply != drtioaux::Payload::RoutingAck {
            return Err(Error::UnexpectedReply);
        }
        Ok(())
    }

    fn init_buffer_space(destination: u8, linkno: u8) {
        let linkno = linkno as usize;
        unsafe {
            (csr::DRTIO[linkno].destination_write)(destination);
            (csr::DRTIO[linkno].force_destination_write)(1);
            (csr::DRTIO[linkno].o_get_buffer_space_write)(1);
            while (csr::DRTIO[linkno].o_wait_read)() == 1 {}
            info!("[DEST#{}] buffer space is {}",
                destination, (csr::DRTIO[linkno].o_dbg_buffer_space_read)());
            (csr::DRTIO[linkno].force_destination_write)(0);
        }
    }

    fn process_local_errors(linkno: u8) {
        let errors;
        let linkidx = linkno as usize;
        unsafe {
            errors = (csr::DRTIO[linkidx].protocol_error_read)();
            (csr::DRTIO[linkidx].protocol_error_write)(errors);
        }
        if errors != 0 {
            error!("[LINK#{}] error(s) found (0x{:02x}):", linkno, errors);
            if errors & 1 != 0 {
                error!("[LINK#{}] received packet of an unknown type", linkno);
            }
            if errors & 2 != 0 {
                error!("[LINK#{}] received truncated packet", linkno);
            }
            if errors & 4 != 0 {
                error!("[LINK#{}] timeout attempting to get remote buffer space", linkno);
            }
        }
    }

    fn destination_set_up(routing_table: &drtio_routing::RoutingTable,
            up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
            destination: u8, up: bool) {
        let mut up_destinations = up_destinations.borrow_mut();
        up_destinations[destination as usize] = up;
        if up {
            drtio_routing::interconnect_enable(routing_table, 0, destination);
            info!("[DEST#{}] destination is up", destination);
        } else {
            drtio_routing::interconnect_disable(destination);
            info!("[DEST#{}] destination is down", destination);
        }
    }

    pub fn destination_up(up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>, destination: u8) -> bool {
        let up_destinations = up_destinations.borrow();
        up_destinations[destination as usize]
    }

    fn destination_survey(io: &Io, routing_table: &drtio_routing::RoutingTable,
            link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>,
            up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
            ddma_mutex: &Mutex, subkernel_mutex: &Mutex) {
        for destination in 0..drtio_routing::DEST_COUNT {
            let hop = routing_table.0[destination][0];
            let destination = destination as u8;

            if hop == 0 {
                /* local RTIO */
                if !destination_up(up_destinations, destination) {
                    destination_set_up(routing_table, up_destinations, destination, true);
                }
            } else if hop as usize <= csr::DRTIO.len() {
                let linkno = hop - 1;
                if destination_up(up_destinations, destination) {
                    let link_up = link_states.borrow()[linkno as usize];
                    if link_up {
                        // eventually todo: schedule transactions first, then get results
                        let reply = aux_transact(io, destination,
                            &drtioaux::Payload::DestinationStatusRequest);
                        if let Ok(reply) = reply {
                            match reply {
                                drtioaux::Payload::DestinationDownReply => {
                                    destination_set_up(routing_table, up_destinations, destination, false);
                                    remote_dma::destination_changed(io, ddma_mutex, destination, false);
                                    subkernel::destination_changed(io, subkernel_mutex, destination, false);
                                }
                                drtioaux::Payload::DestinationOkReply => (),
                                drtioaux::Payload::DestinationSequenceErrorReply { channel } => {
                                    error!("[DEST#{}] RTIO sequence error involving channel 0x{:04x}:{}", destination, channel, resolve_channel_name(channel as u32));
                                    unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_SEQUENCE_ERROR };
                                }
                                drtioaux::Payload::DestinationCollisionReply { channel } => {
                                    error!("[DEST#{}] RTIO collision involving channel 0x{:04x}:{}", destination, channel, resolve_channel_name(channel as u32));
                                    unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_COLLISION };
                                }
                                drtioaux::Payload::DestinationBusyReply { channel } => {
                                    error!("[DEST#{}] RTIO busy error involving channel 0x{:04x}:{}", destination, channel, resolve_channel_name(channel as u32));
                                    unsafe { SEEN_ASYNC_ERRORS |= ASYNC_ERROR_BUSY };
                                }
                                packet => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                                
                            }
                        } else {
                            error!("[DEST#{}] communication failed ({:?})", destination, reply.unwrap_err()); 
                        }
                    } else {
                        destination_set_up(routing_table, up_destinations, destination, false);
                        remote_dma::destination_changed(io, ddma_mutex, destination, false);
                        subkernel::destination_changed(io, subkernel_mutex, destination, false);
                    }
                } else if link_states.borrow()[linkno as usize] {
                    let reply = aux_transact(io, destination,
                        &drtioaux::Payload::DestinationStatusRequest);
                    match reply {
                        Ok(drtioaux::Payload::DestinationDownReply) => (),
                        Ok(drtioaux::Payload::DestinationOkReply) => {
                            destination_set_up(routing_table, up_destinations, destination, true);
                            init_buffer_space(destination as u8, linkno);
                            remote_dma::destination_changed(io, ddma_mutex, destination, true);
                            subkernel::destination_changed(io, subkernel_mutex, destination, true);
                        },
                        Ok(packet) => error!("[DEST#{}] received unexpected aux packet: {:?}", destination, packet),
                        Err(e) => error!("[DEST#{}] communication failed ({:?})", destination, e)
                    }
                }
            }
        }
    }

    fn link_thread(io: Io, routing_table: &drtio_routing::RoutingTable,
            link_states: &Urc<RefCell<[bool; csr::DRTIO.len()]>>,
            up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
            ddma_mutex: &Mutex, subkernel_mutex: &Mutex) {
        loop {
            for linkno in 0..csr::DRTIO.len() {
                let linkno = linkno as u8;
                let link_up = link_states.borrow()[linkno as usize];
                if link_up {
                    /* link was previously up */
                    if link_rx_up(linkno) {
                        process_local_errors(linkno);
                    } else {
                        info!("[LINK#{}] link is down", linkno);
                        link_states.borrow_mut()[linkno as usize] = false;
                    }
                } else {
                    /* link was previously down */
                    if link_rx_up(linkno) {
                        info!("[LINK#{}] link RX became up, pinging", linkno);
                        let ping_count = ping_remote(&io, linkno);
                        if ping_count > 0 {
                            info!("[LINK#{}] remote replied after {} packets", linkno, ping_count);
                            if let Err(e) = sync_tsc(&io, linkno) {
                                error!("[LINK#{}] failed to sync TSC ({:?})", linkno, e);
                            }
                            if let Err(e) = load_routing_table(&io, linkno, routing_table) {
                                error!("[LINK#{}] failed to load routing table ({:?})", linkno, e);
                            }
                            if let Err(e) = set_rank(&io, linkno, 1) {
                                error!("[LINK#{}] failed to set rank ({:?})", linkno, e);
                            }
                            link_states.borrow_mut()[linkno as usize] = true;
                            info!("[LINK#{}] link initialization completed", linkno);
                        } else {
                            error!("[LINK#{}] ping failed", linkno);
                        }
                    }
                }
            }
            destination_survey(&io, routing_table, link_states, up_destinations, ddma_mutex, subkernel_mutex);
            io.sleep(20000).unwrap();
        }
    }

    pub fn reset(io: &Io) {
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(1);
            }
        }
        io.sleep(1).unwrap();
        for linkno in 0..csr::DRTIO.len() {
            unsafe {
                (csr::DRTIO[linkno].reset_write)(0);
            }
        }

        let mut handles = [0; csr::DRTIO.len()];
        for linkno in 0..csr::DRTIO.len() {
            let linkno = linkno as u8;
            if link_rx_up(linkno) {
                // schedule resets first
                handles[linkno as usize] = unsafe { TRANSACTION_MANAGER.transact_async(
                    0, 
                    &drtioaux::Payload::ResetRequest,
                    Some(linkno)).unwrap() };
            }
        }
        for linkno in 0..csr::DRTIO.len() {
            if link_rx_up(linkno as u8) {
                // check replies now
                let reply = await_transaction(io, handles[linkno]);
                match reply {
                    Ok(drtioaux::Payload::ResetAck) => (),
                    Ok(_) => error!("[LINK#{}] reset failed, received unexpected aux packet", linkno),
                    Err(e) => error!("[LINK#{}] reset failed, aux packet error ({:?})", linkno, e)
                }
            }
        }
    }

    pub fn partition_data<F>(data: &[u8], send_f: F) -> Result<(), Error>
            where F: Fn(&[u8; MASTER_PAYLOAD_MAX_SIZE], PayloadStatus, usize) -> Result<(), Error> {
            let mut i = 0;
            while i < data.len() {
                let mut slice: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
                let len: usize = if i + MASTER_PAYLOAD_MAX_SIZE < data.len() { MASTER_PAYLOAD_MAX_SIZE } else { data.len() - i } as usize;
                let first = i == 0;
                let last = i + len == data.len();
                let status = PayloadStatus::from_status(first, last);
                slice[..len].clone_from_slice(&data[i..i+len]);
                i += len;
                send_f(&slice, status, len)?;
            }
            Ok(())
        }

}

#[cfg(not(has_drtio))]
pub mod drtio {
    use super::*;

    pub fn startup(_io: &Io, _routing_table: &Urc<RefCell<drtio_routing::RoutingTable>>,
        _up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        _ddma_mutex: &Mutex, _subkernel_mutex: &Mutex) {}
    pub fn reset(_io: &Io) {}
}

static mut SEEN_ASYNC_ERRORS: u8 = 0;

pub unsafe fn get_async_errors() -> u8 {
    let errors = SEEN_ASYNC_ERRORS;
    SEEN_ASYNC_ERRORS = 0;
    errors
}

fn async_error_thread(io: Io) {
    loop {
        unsafe {
            io.until(|| csr::rtio_core::async_error_read() != 0).unwrap();
            let errors = csr::rtio_core::async_error_read();
            if errors & ASYNC_ERROR_COLLISION != 0 {
                let channel = csr::rtio_core::collision_channel_read();
                error!("RTIO collision involving channel 0x{:04x}:{}", channel, resolve_channel_name(channel as u32));
            }
            if errors & ASYNC_ERROR_BUSY != 0 {
                let channel = csr::rtio_core::busy_channel_read();
                error!("RTIO busy error involving channel 0x{:04x}:{}", channel, resolve_channel_name(channel as u32));
            }
            if errors & ASYNC_ERROR_SEQUENCE_ERROR != 0 {
                let channel = csr::rtio_core::sequence_error_channel_read();
                error!("RTIO sequence error involving channel 0x{:04x}:{}", channel, resolve_channel_name(channel as u32));
            }
            SEEN_ASYNC_ERRORS = errors;
            csr::rtio_core::async_error_write(errors);
        }
    }
}

fn read_device_map() -> DeviceMap {
    let mut device_map: DeviceMap = DeviceMap::new();
    config::read("device_map", |value: Result<&[u8], config::Error>| {
        let mut bytes = match value {
            Ok(val) => if val.len() > 0 { Cursor::new(val) } else {
                warn!("device map not found in config, device names will not be available in RTIO error messages");
                return;
            },
            Err(err) => {
                warn!("error reading device map ({}), device names will not be available in RTIO error messages", err);
                return;
            }
        };
        let size = bytes.read_u32().unwrap();
        for _ in 0..size {
            let channel = bytes.read_u32().unwrap();
            let device_name= bytes.read_string().unwrap();
            if let Some(old_entry) = device_map.insert(channel, device_name.clone()) {
                warn!("conflicting device map entries for RTIO channel {}: '{}' and '{}'",
                       channel, old_entry, device_name);
            }
        }
    });
    device_map
}

pub fn startup(io: &Io, routing_table: &Urc<RefCell<drtio_routing::RoutingTable>>,
        up_destinations: &Urc<RefCell<[bool; drtio_routing::DEST_COUNT]>>,
        ddma_mutex: &Mutex, subkernel_mutex: &Mutex) {
    set_device_map(read_device_map());
    drtio::startup(io, routing_table, up_destinations, ddma_mutex, subkernel_mutex);
    unsafe {
        csr::rtio_core::reset_phy_write(1);
    }
    io.spawn(4096, async_error_thread);
}

pub fn reset(io: &Io) {
    unsafe {
        csr::rtio_core::reset_write(1);
    }
    drtio::reset(io)
}
