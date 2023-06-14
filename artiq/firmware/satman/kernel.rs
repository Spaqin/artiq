use core::mem;
use alloc::{string::String, format};

use board_artiq::mailbox;

use proto_artiq::kernel_proto as kern;
use board_misoc::{csr, clock};

mod kernel {
    use core::ptr;
    use board_misoc::csr;
    use board_artiq::{mailbox, rpc_queue};

    use proto_artiq::kernel_proto::{KERNELCPU_EXEC_ADDRESS, KERNELCPU_LAST_ADDRESS, KSUPPORT_HEADER_SIZE};

    pub unsafe fn start() {
        if csr::kernel_cpu::reset_read() == 0 {
            panic!("attempted to start kernel CPU when it is already running")
        }

        stop();

        csr::cri_con::selected_write(2);
        extern {
            static _binary____ksupport_ksupport_elf_start: u8;
            static _binary____ksupport_ksupport_elf_end: u8;
        }
        let ksupport_start = &_binary____ksupport_ksupport_elf_start as *const _;
        let ksupport_end   = &_binary____ksupport_ksupport_elf_end as *const _;
        ptr::copy_nonoverlapping(ksupport_start,
                                (KERNELCPU_EXEC_ADDRESS - KSUPPORT_HEADER_SIZE) as *mut u8,
                                ksupport_end as usize - ksupport_start as usize);

        let ptr = (KERNELCPU_EXEC_ADDRESS - KSUPPORT_HEADER_SIZE) as *mut u8;
        for i in 0..(ksupport_end as usize - ksupport_start as usize) {
            let j = i as isize;
            if *ptr.offset(j) != *ksupport_start.offset(j) {
                info!("mismatch in memory at: {}", i);
            }
        }
        csr::kernel_cpu::reset_write(0);

        rpc_queue::init();
    }

    pub unsafe fn stop() {
        csr::kernel_cpu::reset_write(1);
        csr::cri_con::selected_write(0);

        mailbox::acknowledge();
        rpc_queue::init();
    }

    pub fn validate(ptr: usize) -> bool {
        ptr >= KERNELCPU_EXEC_ADDRESS && ptr <= KERNELCPU_LAST_ADDRESS
    }

}


#[derive(Debug)]
pub enum Error{
    Load(String),
    KernelNotFound,
    InvalidPointer(usize),
    ClockFailure,
    Unexpected(String),
}

macro_rules! unexpected {
     ($($arg:tt)*) => (return Err(Error::Unexpected(format!($($arg)*))));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KernelState {
    Absent,
    Loaded,
    Running
}

// Per-connection state
#[derive(Debug)]
pub struct Session {
    // congress: &'a mut Congress,
    kernel_state: KernelState,
    log_buffer: String
}

impl Session {
    pub fn new() -> Session {
        Session {
            // congress: congress,
            kernel_state: KernelState::Absent,
            log_buffer: String::new()
        }
    }

    fn running(&self) -> bool {
        match self.kernel_state {
            KernelState::Absent  | KernelState::Loaded  => false,
            KernelState::Running => true
        }
    }

    fn flush_log_buffer(&mut self) {
        if &self.log_buffer[self.log_buffer.len() - 1..] == "\n" {
            for line in self.log_buffer.lines() {
                info!(target: "kernel", "{}", line);
            }
            self.log_buffer.clear()
        }
    }
}

fn kern_recv_notrace<R, F>(f: F) -> Result<R, Error>
        where F: FnOnce(&kern::Message) -> Result<R, Error> {
    info!("waiting for mailbox rcv");
    while mailbox::receive() == 0 {};
    info!("YOU GOT MAIL");
    if !kernel::validate(mailbox::receive()) {
        return Err(Error::InvalidPointer(mailbox::receive()))
    }

    f(unsafe { &*(mailbox::receive() as *const kern::Message) })
}

fn kern_recv_dotrace(reply: &kern::Message) {
    match reply {
        &kern::Log(_) => debug!("comm<-kern Log(...)"),
        &kern::LogSlice(_) => debug!("comm<-kern LogSlice(...)"),
        &kern::DmaRecordAppend(data) => {
            if data.len() > 100 {
                debug!("comm<-kern DmaRecordAppend([_; {:#x}])", data.len())
            } else {
                debug!("comm<-kern {:?}", reply)
            }
        }
        _ => debug!("comm<-kern {:?}", reply)
    }
}

#[inline(always)]
fn kern_recv<R, F>(f: F) -> Result<R, Error>
        where F: FnOnce(&kern::Message) -> Result<R, Error> {
    kern_recv_notrace(|reply| {
        kern_recv_dotrace(reply);
        f(reply)
    })
}

pub fn kern_acknowledge() -> Result<(), Error> {
    mailbox::acknowledge();
    Ok(())
}

pub fn kern_run(session: &mut Session) -> Result<(), Error> {
    if session.kernel_state != KernelState::Loaded {
        unexpected!("attempted to run a kernel while not in Loaded state")
    }

    session.kernel_state = KernelState::Running;

    kern_acknowledge()
}

pub fn kern_send(request: &kern::Message) -> Result<(), Error> {
    match request {
        &kern::LoadRequest(_) => debug!("comm->kern LoadRequest(...)"),
        &kern::DmaRetrieveReply { trace, duration, uses_ddma } => {
            if trace.map(|data| data.len() > 100).unwrap_or(false) {
                debug!("comm->kern DmaRetrieveReply {{ trace: ..., duration: {:?}, uses_ddma: {} }}", duration, uses_ddma)
            } else {
                debug!("comm->kern {:?}", request)
            }
        }
        _ => debug!("comm->kern {:?}", request)
    }
    unsafe { mailbox::send(request as *const _ as usize) }
    loop {
        if mailbox::acknowledged() {
            info!("mailbox acknowledged!");
            break;
        }
    }
    Ok(())
}

pub unsafe fn kern_load(session: &mut Session, library: &[u8])
                   -> Result<(), Error> {
    if session.running() {
        unexpected!("attempted to load a new kernel while a kernel was running")
    }

    kernel::start();

    kern_send(&kern::LoadRequest(&library)).unwrap();
    info!("load request sent");
    kern_recv(|reply| {
        match reply {
            kern::LoadReply(Ok(())) => {
                info!("kern loaded ok");
                session.kernel_state = KernelState::Loaded;
                Ok(())
            }
            kern::LoadReply(Err(error)) => {
                info!("kern loaded wrong");
                kernel::stop();
                Err(Error::Load(format!("{}", error)))
            }
            other => {
                info!("unexpected");
                unexpected!("unexpected kernel CPU reply to load request: {:?}", other)
            }
        }
    })
}

pub fn process_kern_message(session: &mut Session) -> Result<bool, Error> {
    kern_recv_notrace(|request| {
        match (request, session.kernel_state) {
            (&kern::LoadReply(_), KernelState::Loaded) => {
                // We're standing by; ignore the message.
                return Ok(false)
            }
            (_, KernelState::Running) => (),
            _ => {
                unexpected!("unexpected request {:?} from kernel CPU in {:?} state",
                            request, session.kernel_state)
            }
        }

        kern_recv_dotrace(request);

        if process_kern_hwreq(request)? {
            return Ok(false)
        }

        match request {
            &kern::Log(args) => {
                use core::fmt::Write;
                session.log_buffer
                       .write_fmt(args)
                       .unwrap_or_else(|_| warn!("cannot append to session log buffer"));
                session.flush_log_buffer();
                kern_acknowledge()
            }

            &kern::LogSlice(arg) => {
                session.log_buffer += arg;
                session.flush_log_buffer();
                kern_acknowledge()
            }

            &kern::DmaRecordStart(_name) => {
                kern_acknowledge()
            }
            &kern::DmaRecordAppend(_data) => {
                kern_acknowledge()
            }
            &kern::DmaRecordStop { duration: _dur, enable_ddma: _ } => {
                kern_acknowledge()
            }
            &kern::DmaEraseRequest { name: _name } => {
                kern_acknowledge()
            }
            &kern::DmaRetrieveRequest { name: _name } => {
                kern_send(&kern::DmaRetrieveReply {
                    trace:    None,
                    duration: 0,
                    uses_ddma: false,
                })
            }
            &kern::DmaStartRemoteRequest { id: _id, timestamp: _timestamp } => {
                kern_acknowledge()
            }
            &kern::DmaAwaitRemoteRequest { id: _id } => {
                let reply = kern::DmaAwaitRemoteReply { timeout: false, error: 0, channel: 0, timestamp: 0};
                kern_send(&reply)
            }

            &kern::RpcSend { async: _async, service: _service, tag: _tag, data: _data } => {
                kern_acknowledge()
            },
            &kern::RpcFlush => {
                // See ksupport/lib.rs for the reason this request exists.
                // We do not need to do anything here because of how the main loop is
                // structured.
                kern_acknowledge()
            },

            &kern::CacheGetRequest { key: _key } => {
                kern_send(&kern::CacheGetReply {
                    // Zing! This transmute is only safe because we dynamically track
                    // whether the kernel has borrowed any values from the cache.
                    value: unsafe { mem::transmute([0]) }
                })
            }

            &kern::CachePutRequest { key: _key, value: _value } => {
                kern_send(&kern::CachePutReply { succeeded: false })
            }

            &kern::RunFinished => {
                unsafe { kernel::stop() }
                session.kernel_state = KernelState::Absent;

                info!("kernel finished");
                Ok(())
            }
            &kern::RunException {
                exceptions,
                stack_pointers,
                backtrace
            } => {
                unsafe { kernel::stop() }
                session.kernel_state = KernelState::Absent;

                error!("exception in flash kernel");
                for exception in exceptions {
                    error!("{:?}", exception.unwrap());
                }
                return Ok(true)
            }

            request => unexpected!("unexpected request {:?} from kernel CPU", request)
        }.and(Ok(false))
    })
}

pub fn process_kern_hwreq(request: &kern::Message) -> Result<bool, Error> {
match request {
    &kern::RtioInitRequest => {
        info!("resetting RTIO");
        unsafe {
            csr::drtiosat::reset_write(1);
            clock::spin_us(100);
            csr::drtiosat::reset_write(0);
        }
        kern_acknowledge()
    }

    &kern::RtioDestinationStatusRequest { destination: _destination } => {
        kern_send(&kern::RtioDestinationStatusReply { up: true })
    }

    &kern::I2cStartRequest { busno } => {
        kern_send(&kern::I2cBasicReply { succeeded: false })
    }
    &kern::I2cRestartRequest { busno } => {
        kern_send(&kern::I2cBasicReply { succeeded: false})
    }
    &kern::I2cStopRequest { busno } => {
        kern_send(&kern::I2cBasicReply { succeeded: false })
    }
    &kern::I2cWriteRequest { busno, data } => {
        kern_send(&kern::I2cWriteReply { succeeded: false, ack: false })
    }
    &kern::I2cReadRequest { busno, ack } => {
        kern_send(&kern::I2cReadReply { succeeded: false, data: 0xff })
    }
    &kern::I2cSwitchSelectRequest { busno, address, mask } => {
        kern_send(&kern::I2cBasicReply { succeeded: false })
    }

    &kern::SpiSetConfigRequest { busno, flags, length, div, cs } => {
        kern_send(&kern::SpiBasicReply { succeeded: false })
    },
    &kern::SpiWriteRequest { busno, data } => {
        kern_send(&kern::SpiBasicReply { succeeded: false })
    }
    &kern::SpiReadRequest { busno } => {
        kern_send(&kern::SpiReadReply { succeeded: false, data: 0 })
    }

    _ => return Ok(false)
}.and(Ok(true))
}