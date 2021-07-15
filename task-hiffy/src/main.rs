//! HIF interpreter

#![no_std]
#![no_main]

use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};
use hif::*;
use ringbuf::*;
use userlib::*;
use drv_i2c_api::{Controller, Port, Mux, Segment, ResponseCode, I2cDevice};
use task_jefe_api::{Disposition, JefeError, Jefe};

#[cfg(feature = "standalone")]
const I2C: Task = Task::anonymous;

#[cfg(not(feature = "standalone"))]
const I2C: Task = Task::i2c_driver;
const JEFE: Task = Task::jefe;

#[no_mangle]
static mut HIFFY_TEXT: [u8; 2048] = [0; 2048];
static mut HIFFY_RSTACK: [u8; 2048] = [0; 2048];
static HIFFY_REQUESTS: AtomicU32 = AtomicU32::new(0);
static HIFFY_ERRORS: AtomicU32 = AtomicU32::new(0);
static HIFFY_KICK: AtomicU32 = AtomicU32::new(0);
static HIFFY_READY: AtomicU32 = AtomicU32::new(0);

#[derive(Copy, Clone, PartialEq)]
enum Trace {
    Execute((usize, Op)),
    Function(u32),
    Failure(Failure),
    Success,
    None,
}

ringbuf!(Trace, 64, Trace::None);

//
// The order in this enum must match the order in the functions array that
// is passed to execute.
//
pub enum Functions {
    I2cRead(
        (Controller, Port, Mux, Segment, u8, u8, usize),
        ResponseCode,
    ),
    I2cWrite((Controller, Port, Mux, Segment, u8, u8), ResponseCode),
    JefeSetDisposition((u16, Disposition), JefeError),
}

//
// This definition forces the compiler to emit the DWARF needed for debuggers
// to be able to know function indices, arguments and return values.
//
#[no_mangle]
static HIFFY_FUNCTIONS: Option<&Functions> = None;

fn i2c_read(stack: &[Option<u32>], rval: &mut [u8]) -> Result<usize, Failure> {
    if stack.len() < 7 {
        return Err(Failure::Fault(Fault::MissingParameters));
    }

    let fp = stack.len() - 7;

    let controller = match stack[fp + 0] {
        Some(controller) => match Controller::from_u32(controller) {
            Some(controller) => controller,
            None => return Err(Failure::Fault(Fault::BadParameter(0))),
        },
        None => return Err(Failure::Fault(Fault::EmptyParameter(0))),
    };

    let port = match stack[fp + 1] {
        Some(port) => match Port::from_u32(port) {
            Some(port) => port,
            None => {
                return Err(Failure::Fault(Fault::BadParameter(1)));
            }
        },
        None => Port::Default,
    };

    let mux = match (stack[fp + 2], stack[fp + 3]) {
        (Some(mux), Some(segment)) => Some((
            match Mux::from_u32(mux) {
                Some(mux) => mux,
                None => {
                    return Err(Failure::Fault(Fault::BadParameter(2)));
                }
            },
            match Segment::from_u32(segment) {
                Some(segment) => segment,
                None => {
                    return Err(Failure::Fault(Fault::BadParameter(3)));
                }
            },
        )),
        _ => None,
    };

    let addr = match stack[fp + 4] {
        Some(addr) => addr as u8,
        None => return Err(Failure::Fault(Fault::EmptyParameter(4))),
    };

    let task = TaskId::for_index_and_gen(I2C as usize, Generation::default());
    let device = I2cDevice::new(task, controller, port, mux, addr);

    if rval.len() < 1 {
        return Err(Failure::Fault(Fault::ReturnValueOverflow));
    }

    let register = stack[fp + 5];

    match stack[fp + 6] {
        Some(1) => {
            let result = match register {
                Some(reg) => device.read_reg::<u8, u8>(reg as u8),
                None => device.read::<u8>(),
            };

            match result {
                Ok(result) => {
                    rval[0] = result;
                    Ok(1)
                }
                Err(err) => Err(Failure::FunctionError(err.into())),
            }
        }

        Some(2) => {
            let result = match register {
                Some(reg) => device.read_reg::<u8, [u8; 2]>(reg as u8),
                None => device.read::<[u8; 2]>(),
            };

            match result {
                Ok(result) => {
                    rval[0] = result[0];
                    rval[1] = result[1];
                    Ok(2)
                }
                Err(err) => Err(Failure::FunctionError(err.into())),
            }
        }

        Some(_) => Err(Failure::Fault(Fault::BadParameter(5))),

        None => {
            if let Some(reg) = register {
                if rval.len() < 256 {
                    return Err(Failure::Fault(Fault::ReturnValueOverflow));
                }

                match device.read_block::<u8>(reg as u8, &mut rval[0..0xff]) {
                    Ok(rlen) => Ok(rlen),
                    Err(err) => Err(Failure::FunctionError(err.into())),
                }
            } else {
                Err(Failure::Fault(Fault::EmptyParameter(6)))
            }
        }
    }
}

fn i2c_write(_stack: &[Option<u32>], _rval: &mut [u8]) -> Result<usize, Failure> {
    Err(Failure::Fault(Fault::MissingParameters))
}

fn jefe_set_disposition(
    stack: &[Option<u32>],
    _rval: &mut [u8]
) -> Result<usize, Failure> {
    if stack.len() < 2 {
        return Err(Failure::Fault(Fault::MissingParameters));
    }

    let fp = stack.len() - 2;

    let task = match stack[fp + 0] {
        Some(task) => task as u16,
        None => return Err(Failure::Fault(Fault::EmptyParameter(0))),
    };

    let disposition = match stack[fp + 1] {
        Some(disposition) => match Disposition::from_u32(disposition) {
            Some(disposition) => disposition,
            None => {
                return Err(Failure::Fault(Fault::BadParameter(1)));
            }
        },
        None => return Err(Failure::Fault(Fault::EmptyParameter(1))),
    };

    let jefe = Jefe(
        TaskId::for_index_and_gen(JEFE as usize, Generation::default())
    );

    match jefe.set_disposition(TaskId(task), disposition) {
        Ok(_) => Ok(0),
        Err(err) => Err(Failure::FunctionError(err.into())),
    }
}

#[export_name = "main"]
fn main() -> ! {
    let mut sleep_ms = 250;
    let mut sleeps = 0;
    let functions: &[Function] = &[i2c_read, i2c_write, jefe_set_disposition];
    let mut stack = [None; 8];
    let mut scratch = [0u8; 256];

    loop {
        HIFFY_READY.fetch_add(1, Ordering::SeqCst);
        hl::sleep_for(sleep_ms);
        HIFFY_READY.fetch_sub(1, Ordering::SeqCst);

        if HIFFY_KICK.load(Ordering::SeqCst) == 0 {
            sleeps += 1;

            // Exponentially backoff our sleep value, but no more than 250ms
            if sleeps == 10 {
                sleep_ms = core::cmp::min(sleep_ms * 10, 250);
                sleeps = 0;
            }

            continue;
        }

        //
        // Whenever we have been kicked, we adjust our timeout down to 1ms,
        // from which we will exponentially backoff
        //
        HIFFY_KICK.fetch_sub(1, Ordering::SeqCst);
        sleep_ms = 1;
        sleeps = 0;

        let text = unsafe { &HIFFY_TEXT };
        let mut rstack = unsafe { &mut HIFFY_RSTACK[0..] };

        let check = |offset: usize, op: &Op| -> Result<(), Failure> {
            ringbuf_entry!(Trace::Execute((offset, *op)));
            Ok(())
        };

        let rv = execute(
            text,
            functions,
            &mut stack,
            &mut rstack,
            &mut scratch,
            check,
        );

        match rv {
            Ok(_) => {
                HIFFY_REQUESTS.fetch_add(1, Ordering::SeqCst);
                ringbuf_entry!(Trace::Success);
            }
            Err(failure) => {
                HIFFY_ERRORS.fetch_add(1, Ordering::SeqCst);
                ringbuf_entry!(Trace::Failure(failure));
            }
        }
    }
}
