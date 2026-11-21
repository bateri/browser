//! Frames: one IOSurface per mach message, carried as a send right.
//!
//! bateri owns the receive right under a bootstrap name it made up (`Launch::mach_name`); the
//! helper looks the name up and sends a `Frame` beside the surface of what Chromium just drew.
//! Nothing is copied between the processes, and the helper never waits on bateri: a full queue
//! drops the frame (`Sent::Full`) and the helper sends the newest one again later.

use std::ffi::{CString, c_void};
use std::io;
use std::mem::size_of;

use mach2::bootstrap::{bootstrap_check_in, bootstrap_look_up, bootstrap_port, bootstrap_register};
use mach2::kern_return::KERN_SUCCESS;
use mach2::mach_port::{mach_port_allocate, mach_port_deallocate, mach_port_insert_right};
use mach2::message::*;
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::traps::mach_task_self;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceCreateMachPort(surface: *mut c_void) -> mach_port_t;
    fn IOSurfaceLookupFromMachPort(port: mach_port_t) -> *mut c_void;
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    fn IOSurfaceGetBytesPerRow(surface: *mut c_void) -> usize;
    fn IOSurfaceGetBaseAddress(surface: *mut c_void) -> *mut c_void;
    fn IOSurfaceLock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: *mut c_void, options: u32, seed: *mut u32) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFRetain(cf: *const c_void) -> *const c_void;
}

/// Bootstrap names start with this; `Launch` appends a random token.
pub const NAME_PREFIX: &str = "dev.bateri.browser-frames";

/// A frame of the page itself.
pub const KIND_VIEW: u32 = 0;
/// A frame of a popup (a `<select>` list) that goes over the page at `x`, `y`.
pub const KIND_POPUP: u32 = 1;

/// What travels beside the port. Plain integers: the layout is the wire format.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Frame {
    /// The browser the frame belongs to (the number of `Command::Open`).
    pub browser: u32,
    pub kind: u32,
    /// The surface's size in device pixels.
    pub width: u32,
    pub height: u32,
    /// Popup: where in the view (device pixels) it goes.
    pub x: i32,
    pub y: i32,
    /// Counts per helper, from 1: a gap is a dropped frame.
    pub seq: u32,
    /// Wall clock (µs since the Unix epoch) when the helper sent the frame.
    pub sent_us: u64,
}

#[repr(C)]
struct Message {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    port: mach_msg_port_descriptor_t,
    frame: Frame,
}

#[repr(C)]
struct Incoming {
    message: Message,
    trailer: mach_msg_audit_trailer_t,
}

/// An IOSurface this process holds a reference to.
pub struct Surface(*mut c_void);

// An IOSurfaceRef is a reference-counted object that may be used from any thread.
unsafe impl Send for Surface {}

impl Surface {
    /// Takes a reference of `raw` (an `IOSurfaceRef`) of its own.
    ///
    /// # Safety
    /// `raw` must be a live `IOSurfaceRef`.
    pub unsafe fn retain(raw: *mut c_void) -> Self {
        Self(unsafe { CFRetain(raw) } as *mut c_void)
    }

    pub fn as_ptr(&self) -> *mut c_void {
        self.0
    }

    /// Width and height in pixels.
    pub fn size(&self) -> (usize, usize) {
        unsafe { (IOSurfaceGetWidth(self.0), IOSurfaceGetHeight(self.0)) }
    }

    /// Reads BGRA pixels at `points` (x, y) straight from the shared memory.
    pub fn read(&self, points: &[(usize, usize)]) -> Vec<[u8; 4]> {
        unsafe {
            // kIOSurfaceLockReadOnly
            IOSurfaceLock(self.0, 1, std::ptr::null_mut());
            let base = IOSurfaceGetBaseAddress(self.0) as *const u8;
            let stride = IOSurfaceGetBytesPerRow(self.0);
            let (w, h) = self.size();
            let out = points
                .iter()
                .map(|&(x, y)| {
                    let (x, y) = (x.min(w.saturating_sub(1)), y.min(h.saturating_sub(1)));
                    let p = base.add(y * stride + x * 4);
                    [*p, *p.add(1), *p.add(2), *p.add(3)]
                })
                .collect();
            IOSurfaceUnlock(self.0, 1, std::ptr::null_mut());
            out
        }
    }
}

impl Clone for Surface {
    fn clone(&self) -> Self {
        unsafe { Self::retain(self.0) }
    }
}

impl Drop for Surface {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) }
    }
}

fn check(what: &str, code: i32) -> io::Result<()> {
    if code == KERN_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::other(format!("{what}: kern_return {code}")))
    }
}

/// bateri's end: owns the receive right.
pub struct Receiver {
    port: mach_port_t,
}

impl Receiver {
    /// Owns a receive right under the bootstrap name `name`.
    pub fn register(name: &str) -> io::Result<Self> {
        let cname = CString::new(name).map_err(io::Error::other)?;
        unsafe {
            let mut port = MACH_PORT_NULL;
            let check_in = bootstrap_check_in(bootstrap_port, cname.as_ptr(), &mut port);
            if check_in == KERN_SUCCESS {
                return Ok(Self { port });
            }
            check(
                "mach_port_allocate",
                mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port),
            )?;
            check(
                "mach_port_insert_right",
                mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND),
            )?;
            let registered = bootstrap_register(bootstrap_port, cname.as_ptr() as *mut _, port);
            if registered != KERN_SUCCESS {
                return Err(io::Error::other(format!(
                    "bootstrap_check_in: {check_in}; bootstrap_register: {registered}"
                )));
            }
            Ok(Self { port })
        }
    }

    /// Waits up to `timeout_ms` for a frame (`None`: nothing came).
    pub fn recv(&self, timeout_ms: u32) -> io::Result<Option<(Frame, Surface)>> {
        unsafe {
            let mut incoming: Incoming = std::mem::zeroed();
            let code = mach_msg(
                &mut incoming.message.header,
                MACH_RCV_MSG | MACH_RCV_TIMEOUT,
                0,
                size_of::<Incoming>() as u32,
                self.port,
                timeout_ms,
                MACH_PORT_NULL,
            );
            if code == MACH_RCV_TIMED_OUT {
                return Ok(None);
            }
            if code != MACH_MSG_SUCCESS {
                return Err(io::Error::other(format!("mach_msg recv: {code:#x}")));
            }
            let port = incoming.message.port.name;
            let raw = IOSurfaceLookupFromMachPort(port);
            mach_port_deallocate(mach_task_self(), port);
            if raw.is_null() {
                return Err(io::Error::other("IOSurfaceLookupFromMachPort: null"));
            }
            Ok(Some((incoming.message.frame, Surface(raw))))
        }
    }
}

/// What became of a frame the helper tried to send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    Queued,
    /// The queue is full: bateri is behind. Nothing was sent.
    Full,
}

/// The helper's end.
pub struct Sender {
    port: mach_port_t,
}

impl Sender {
    pub fn lookup(name: &str) -> io::Result<Self> {
        let cname = CString::new(name).map_err(io::Error::other)?;
        let mut port = MACH_PORT_NULL;
        let code = unsafe { bootstrap_look_up(bootstrap_port, cname.as_ptr(), &mut port) };
        check("bootstrap_look_up", code)?;
        Ok(Self { port })
    }

    /// Sends `frame` with a fresh send right to `surface` (an `IOSurfaceRef` the caller keeps).
    /// Never waits.
    ///
    /// # Safety
    ///
    /// `surface` must be a valid `IOSurfaceRef`.
    pub unsafe fn send(&self, mut frame: Frame, surface: *mut c_void) -> io::Result<Sent> {
        unsafe {
            let surface_port = IOSurfaceCreateMachPort(surface);
            if surface_port == MACH_PORT_NULL {
                return Err(io::Error::other("IOSurfaceCreateMachPort: null"));
            }
            frame.sent_us = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            let mut message: Message = std::mem::zeroed();
            message.header.msgh_bits = MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND;
            message.header.msgh_size = size_of::<Message>() as u32;
            message.header.msgh_remote_port = self.port;
            message.header.msgh_local_port = MACH_PORT_NULL;
            message.body.msgh_descriptor_count = 1;
            message.port = mach_msg_port_descriptor_t::new(surface_port, MACH_MSG_TYPE_MOVE_SEND);
            message.frame = frame;
            let code = mach_msg(
                &mut message.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size_of::<Message>() as u32,
                0,
                MACH_PORT_NULL,
                0,
                MACH_PORT_NULL,
            );
            if code == MACH_MSG_SUCCESS {
                return Ok(Sent::Queued);
            }
            // The send right in the descriptor is still ours when the send failed.
            mach_port_deallocate(mach_task_self(), surface_port);
            if code == MACH_SEND_TIMED_OUT {
                Ok(Sent::Full)
            } else {
                Err(io::Error::other(format!("mach_msg send: {code:#x}")))
            }
        }
    }
}
