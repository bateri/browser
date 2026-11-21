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
use mach2::mach_port::{
    mach_port_allocate, mach_port_deallocate, mach_port_insert_right, mach_port_mod_refs,
};
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
#[derive(Clone, Copy)]
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

/// Whether a received message has the shape of a frame: complex, one port descriptor that holds a
/// send right, and exactly the bytes of a `Message`. (A right of another kind arrives as another
/// disposition.)
fn is_frame(message: &Message) -> bool {
    message.header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0
        && message.header.msgh_size as usize == size_of::<Message>()
        && message.body.msgh_descriptor_count == 1
        && message.port.type_ == MACH_MSG_PORT_DESCRIPTOR as u8
        && message.port.disposition as mach_msg_type_name_t == MACH_MSG_TYPE_MOVE_SEND
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

/// Gives up the receive right of `port`: the port dies, senders get a dead name, and the name is
/// free again. Nothing else may hold a right under `port` in this task.
fn destroy(port: mach_port_t) {
    unsafe {
        mach_port_mod_refs(mach_task_self(), port, MACH_PORT_RIGHT_RECEIVE, -1);
    }
}

/// bateri's end: owns the receive right, and gives it up when dropped (a helper started later
/// makes a new one under a new name; the old right would otherwise stay until bateri quits).
pub struct Receiver {
    port: mach_port_t,
}

impl Drop for Receiver {
    fn drop(&mut self) {
        destroy(self.port);
    }
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
            if let Err(e) = check(
                "mach_port_insert_right",
                mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND),
            ) {
                // The receive right exists and nobody else will ever give it up.
                destroy(port);
                return Err(e);
            }
            let registered = bootstrap_register(bootstrap_port, cname.as_ptr() as *mut _, port);
            // The send right was only there to register with: the bootstrap server holds its own
            // now, and keeping ours would leave a name behind after the receive right is gone.
            mach_port_deallocate(mach_task_self(), port);
            if registered != KERN_SUCCESS {
                destroy(port);
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
            // A message too large for the buffer was destroyed by the kernel: not a frame.
            if code == MACH_RCV_TIMED_OUT || code == MACH_RCV_TOO_LARGE {
                return Ok(None);
            }
            if code != MACH_MSG_SUCCESS {
                return Err(io::Error::other(format!("mach_msg recv: {code:#x}")));
            }
            // Whoever holds the send right can send anything. What is not a frame is destroyed
            // (that gives back every right it carries, by what the kernel says it carries) and
            // never read: its bytes are not a port name of ours.
            if !is_frame(&incoming.message) {
                mach_msg_destroy(&mut incoming.message.header);
                return Ok(None);
            }
            let port = incoming.message.port.name;
            let raw = IOSurfaceLookupFromMachPort(port);
            mach_port_deallocate(mach_task_self(), port);
            if raw.is_null() {
                // The right was not an IOSurface's.
                return Ok(None);
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

impl Drop for Sender {
    fn drop(&mut self) {
        unsafe {
            mach_port_deallocate(mach_task_self(), self.port);
        }
    }
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
            // The send failed and the message is ours again, rights included: the kernel put the
            // descriptor's send right back under a name of its own choosing (not `surface_port`'s,
            // which the send consumed) and wrote that name into `message`. Destroying the message
            // lets go of the right by the name it holds now; letting go of `surface_port` left
            // one right behind for every frame that was dropped.
            mach_msg_destroy(&mut message.header);
            if code == MACH_SEND_TIMED_OUT {
                Ok(Sent::Full)
            } else {
                Err(io::Error::other(format!("mach_msg send: {code:#x}")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" {
        fn mach_port_type(task: mach_port_t, name: mach_port_t, port_type: *mut u32) -> i32;
    }

    /// Whether this task still has a right under `name`.
    fn has_name(name: mach_port_t) -> bool {
        let mut kind = 0u32;
        unsafe { mach_port_type(mach_task_self(), name, &mut kind) == KERN_SUCCESS }
    }

    #[link(name = "IOSurface", kind = "framework")]
    unsafe extern "C" {
        fn IOSurfaceCreate(properties: *const c_void) -> *mut c_void;
        static kIOSurfaceWidth: *const c_void;
        static kIOSurfaceHeight: *const c_void;
        static kIOSurfaceBytesPerElement: *const c_void;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFNumberCreate(
            allocator: *const c_void,
            kind: isize,
            value: *const c_void,
        ) -> *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        static kCFTypeDictionaryKeyCallBacks: u8;
        static kCFTypeDictionaryValueCallBacks: u8;
    }

    unsafe extern "C" {
        fn mach_port_names(
            task: mach_port_t,
            names: *mut *mut mach_port_t,
            names_count: *mut u32,
            types: *mut *mut u32,
            types_count: *mut u32,
        ) -> i32;
        fn vm_deallocate(task: mach_port_t, address: usize, size: usize) -> i32;
        fn mach_port_get_refs(
            task: mach_port_t,
            name: mach_port_t,
            right: u32,
            refs: *mut u32,
        ) -> i32;
    }

    /// How many user references this task holds on the send right under `name`.
    fn send_refs(name: mach_port_t) -> u32 {
        let mut refs = 0u32;
        let code = unsafe { mach_port_get_refs(mach_task_self(), name, 0, &mut refs) };
        if code == KERN_SUCCESS { refs } else { 0 }
    }

    /// How many names this task has for ports: a right the test leaves behind is one more.
    fn names_held() -> usize {
        unsafe {
            let (mut names, mut types) = (std::ptr::null_mut(), std::ptr::null_mut());
            let (mut n, mut t) = (0u32, 0u32);
            assert_eq!(
                mach_port_names(mach_task_self(), &mut names, &mut n, &mut types, &mut t),
                KERN_SUCCESS
            );
            vm_deallocate(
                mach_task_self(),
                names as usize,
                n as usize * size_of::<mach_port_t>(),
            );
            vm_deallocate(
                mach_task_self(),
                types as usize,
                t as usize * size_of::<u32>(),
            );
            n as usize
        }
    }

    /// A real IOSurface (a frame's port is one), `side` pixels square.
    fn surface(side: i32) -> Surface {
        unsafe {
            let number = |v: i32| CFNumberCreate(std::ptr::null(), 3, (&v as *const i32).cast());
            let keys = [kIOSurfaceWidth, kIOSurfaceHeight, kIOSurfaceBytesPerElement];
            let values = [number(side), number(side), number(4)];
            let dict = CFDictionaryCreate(
                std::ptr::null(),
                keys.as_ptr(),
                values.as_ptr(),
                3,
                (&raw const kCFTypeDictionaryKeyCallBacks).cast(),
                (&raw const kCFTypeDictionaryValueCallBacks).cast(),
            );
            let raw = IOSurfaceCreate(dict);
            CFRelease(dict);
            for v in values {
                CFRelease(v);
            }
            assert!(!raw.is_null(), "IOSurfaceCreate");
            Surface(raw)
        }
    }

    /// The tests count this process's port names: one at a time.
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unique(label: &str) -> String {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        format!(
            "{NAME_PREFIX}.test-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// A port of this task's own with a receive right and one send right to it.
    fn own_port() -> mach_port_t {
        unsafe {
            let mut port = MACH_PORT_NULL;
            assert_eq!(
                mach_port_allocate(mach_task_self(), MACH_PORT_RIGHT_RECEIVE, &mut port),
                KERN_SUCCESS
            );
            assert_eq!(
                mach_port_insert_right(mach_task_self(), port, port, MACH_MSG_TYPE_MAKE_SEND),
                KERN_SUCCESS
            );
            port
        }
    }

    fn give_up(port: mach_port_t) {
        unsafe {
            mach_port_deallocate(mach_task_self(), port);
        }
        destroy(port);
    }

    /// Sends a hand-made message to `to` without waiting.
    fn send_raw(to: mach_port_t, message: &mut Message) -> i32 {
        message.header.msgh_size = size_of::<Message>() as u32;
        message.header.msgh_remote_port = to;
        unsafe {
            mach_msg(
                &mut message.header,
                MACH_SEND_MSG | MACH_SEND_TIMEOUT,
                size_of::<Message>() as u32,
                0,
                MACH_PORT_NULL,
                0,
                MACH_PORT_NULL,
            )
        }
    }

    #[test]
    fn dropped_frames_leave_no_right_behind() {
        let _one_at_a_time = serial();
        // The queue holds a few frames; the rest are dropped (`Sent::Full`). Each dropped frame
        // used to leave a send right to a port of the surface in the helper for good.
        let name = unique("leak");
        let receiver = Receiver::register(&name).unwrap();
        let sender = Sender::lookup(&name).unwrap();
        let surface = surface(16);
        let frame = Frame {
            browser: 7,
            ..Default::default()
        };
        let mut queued = 0;
        // Past the queue's limit: from here every send is a dropped frame.
        while unsafe { sender.send(frame, surface.as_ptr()) }.unwrap() == Sent::Queued {
            queued += 1;
        }
        assert!(queued >= 1, "nothing was queued");
        let before = names_held();
        for _ in 0..100 {
            assert_eq!(
                unsafe { sender.send(frame, surface.as_ptr()) }.unwrap(),
                Sent::Full
            );
        }
        assert_eq!(
            names_held(),
            before,
            "the dropped frames left rights behind"
        );
        // What was queued arrives whole.
        let mut got = 0;
        while let Some((f, s)) = receiver.recv(100).unwrap() {
            assert_eq!((f.browser, s.size()), (7, (16, 16)));
            got += 1;
        }
        assert_eq!(got, queued);
    }

    #[test]
    fn a_frame_sent_to_a_dead_receiver_leaves_no_right_behind() {
        let _one_at_a_time = serial();
        let name = unique("dead");
        let receiver = Receiver::register(&name).unwrap();
        let sender = Sender::lookup(&name).unwrap();
        drop(receiver);
        let surface = surface(16);
        // A dead port is the end of the conversation, whichever way the kernel reports it; what
        // matters is that no right is kept.
        let _ = unsafe { sender.send(Frame::default(), surface.as_ptr()) };
        let before = names_held();
        for _ in 0..50 {
            let _ = unsafe { sender.send(Frame::default(), surface.as_ptr()) };
        }
        assert_eq!(names_held(), before);
    }

    #[test]
    fn a_message_that_is_not_a_frame_is_dropped_and_its_bytes_are_no_port_name() {
        let _one_at_a_time = serial();
        let name = unique("junk");
        let receiver = Receiver::register(&name).unwrap();
        let sender = Sender::lookup(&name).unwrap();
        // A right of this task's own that must survive whatever arrives.
        let precious = own_port();
        assert_eq!(send_refs(precious), 1);

        // Plain data: the bytes where a frame has its port's name hold `precious`.
        let mut message: Message = unsafe { std::mem::zeroed() };
        message.header.msgh_bits = MACH_MSG_TYPE_COPY_SEND;
        message.port.name = precious;
        assert_eq!(send_raw(sender.port, &mut message), MACH_MSG_SUCCESS);
        assert!(receiver.recv(1000).unwrap().is_none());
        assert_eq!(
            send_refs(precious),
            1,
            "the message's bytes were taken for a name"
        );

        // A complex message with a send right that is no surface's: nothing is kept, no error.
        let other = own_port();
        let before = send_refs(other);
        let mut message: Message = unsafe { std::mem::zeroed() };
        message.header.msgh_bits = MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND;
        message.body.msgh_descriptor_count = 1;
        message.port = mach_msg_port_descriptor_t::new(other, MACH_MSG_TYPE_MAKE_SEND);
        assert_eq!(send_raw(sender.port, &mut message), MACH_MSG_SUCCESS);
        assert!(receiver.recv(1000).unwrap().is_none());
        assert_eq!(send_refs(other), before, "the right it carried was kept");

        // A complex message of the wrong shape (a send-once right in the descriptor): destroyed.
        let third = own_port();
        let mut message: Message = unsafe { std::mem::zeroed() };
        message.header.msgh_bits = MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND;
        message.body.msgh_descriptor_count = 1;
        message.port = mach_msg_port_descriptor_t::new(third, MACH_MSG_TYPE_MAKE_SEND_ONCE);
        assert_eq!(send_raw(sender.port, &mut message), MACH_MSG_SUCCESS);
        assert!(receiver.recv(1000).unwrap().is_none());

        // And a real frame still comes through after all that junk.
        let surface = surface(8);
        let frame = Frame {
            browser: 3,
            seq: 9,
            ..Default::default()
        };
        assert_eq!(
            unsafe { sender.send(frame, surface.as_ptr()) }.unwrap(),
            Sent::Queued
        );
        let (got, s) = receiver.recv(1000).unwrap().expect("the frame");
        assert_eq!((got.browser, got.seq, s.size()), (3, 9, (8, 8)));

        for port in [precious, other, third] {
            give_up(port);
        }
    }

    #[test]
    fn a_frame_is_told_from_other_messages_by_its_shape() {
        let mut m: Message = unsafe { std::mem::zeroed() };
        m.header.msgh_bits = MACH_MSGH_BITS_COMPLEX | MACH_MSG_TYPE_COPY_SEND;
        m.header.msgh_size = size_of::<Message>() as u32;
        m.body.msgh_descriptor_count = 1;
        m.port = mach_msg_port_descriptor_t::new(5, MACH_MSG_TYPE_MOVE_SEND);
        assert!(is_frame(&m));
        let mut plain = m;
        plain.header.msgh_bits &= !MACH_MSGH_BITS_COMPLEX;
        assert!(!is_frame(&plain));
        let mut short = m;
        short.header.msgh_size -= 4;
        assert!(!is_frame(&short));
        let mut two = m;
        two.body.msgh_descriptor_count = 2;
        assert!(!is_frame(&two));
        let mut other = m;
        other.port.type_ = 2;
        assert!(!is_frame(&other));
        let mut receive = m;
        receive.port.disposition = 16;
        assert!(!is_frame(&receive));
    }

    #[test]
    fn a_dropped_receiver_gives_its_port_up() {
        let _one_at_a_time = serial();
        let name = format!("{NAME_PREFIX}.test-{}", std::process::id());
        let receiver = Receiver::register(&name).unwrap();
        let port = receiver.port;
        assert!(has_name(port), "the receive right is there while it lives");
        // A sender in this very process gets a name for the same port (a task has one name per
        // port); its right goes when it is dropped, or the name would outlive the receiver as a
        // dead name.
        let sender = Sender::lookup(&name).unwrap();
        assert_eq!(sender.port, port);
        drop(sender);
        assert!(has_name(port), "the receive right outlives the sender");
        drop(receiver);
        assert!(
            !has_name(port),
            "no right is left under the receiver's name"
        );
    }

    #[test]
    fn a_receiver_per_name_leaves_nothing_behind() {
        let _one_at_a_time = serial();
        // What a long-running bateri does: one receiver per helper it starts.
        let mut names = Vec::new();
        for n in 0..50 {
            let name = format!("{NAME_PREFIX}.test-{}-{n}", std::process::id());
            let receiver = Receiver::register(&name).unwrap();
            names.push(receiver.port);
        }
        // Every receiver was dropped at the end of its turn; none of the names is still ours.
        assert!(names.iter().all(|&p| !has_name(p)));
    }
}
