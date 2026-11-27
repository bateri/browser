//! The control socket: framing, the handshake and both ends' typed handles.
//!
//! bateri owns the listening socket (a `0600` socket in a `0700` directory it made), starts the
//! helper with the single-use key, and accepts one connection. The helper connects and says
//! `Event::Hello` first; a wrong key, a wrong protocol or a peer that is not the process bateri
//! started is refused. Messages are a big-endian `u32` length and that many bytes of JSON; a
//! length above the bound closes the conversation.

use crate::message::{Command, Event, PROTOCOL};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::io::{self, Read, Write};
use std::marker::PhantomData;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest message either side takes: a full-page screenshot is the biggest thing sent.
pub const MAX_MESSAGE: usize = 64 * 1024 * 1024;

/// The largest first message (the hello is a few hundred bytes).
const MAX_HELLO: usize = 4096;

/// Writes one message.
pub fn write_message<W: Write, T: Serialize>(out: &mut W, message: &T) -> io::Result<()> {
    let body = serde_json::to_vec(message).map_err(io::Error::other)?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "message too large",
        ));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    out.write_all(&frame)?;
    out.flush()
}

/// Reads one message; `Ok(None)` when the other side closed between messages.
pub fn read_message<R: Read, T: DeserializeOwned>(
    input: &mut R,
    limit: usize,
) -> io::Result<Option<T>> {
    let mut length = [0u8; 4];
    match input.read(&mut length[..1])? {
        0 => return Ok(None),
        _ => input.read_exact(&mut length[1..])?,
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("message of {length} bytes is over the {limit} byte limit"),
        ));
    }
    // The length is bounded, but a peer that lies about it must not make this allocate it up
    // front: grow with what actually arrives.
    let mut body = Vec::new();
    input.take(length as u64).read_to_end(&mut body)?;
    if body.len() != length {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    serde_json::from_slice(&body)
        .map(Some)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// The sending half. Clones share the socket; each message is written whole.
pub struct Writer<T> {
    stream: Arc<Mutex<UnixStream>>,
    kind: PhantomData<fn(T)>,
}

impl<T> Clone for Writer<T> {
    fn clone(&self) -> Self {
        Self {
            stream: self.stream.clone(),
            kind: PhantomData,
        }
    }
}

impl<T: Serialize> Writer<T> {
    pub fn send(&self, message: &T) -> io::Result<()> {
        let mut stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        write_message(&mut *stream, message)
    }

    /// Closes the conversation for both directions (a reader blocked on it wakes with EOF).
    pub fn shutdown(&self) {
        let stream = self.stream.lock().unwrap_or_else(|e| e.into_inner());
        let _ = stream.shutdown(std::net::Shutdown::Both);
    }
}

/// The receiving half.
pub struct Reader<T> {
    stream: UnixStream,
    limit: usize,
    kind: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Reader<T> {
    /// The next message; `Ok(None)` when the other side hung up.
    pub fn recv(&mut self) -> io::Result<Option<T>> {
        read_message(&mut self.stream, self.limit)
    }
}

fn halves<I, O>(stream: UnixStream, limit: usize) -> io::Result<(Writer<O>, Reader<I>)> {
    let reader = stream.try_clone()?;
    Ok((
        Writer {
            stream: Arc::new(Mutex::new(stream)),
            kind: PhantomData,
        },
        Reader {
            stream: reader,
            limit,
            kind: PhantomData,
        },
    ))
}

/// The pid of the process at the other end of a unix socket.
pub fn peer_pid(stream: &UnixStream) -> io::Result<u32> {
    let mut pid: libc::pid_t = 0;
    let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
    // SOL_LOCAL = 0, LOCAL_PEERPID = 2 (<sys/un.h>).
    let code = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            0,
            2,
            (&mut pid as *mut libc::pid_t).cast(),
            &mut length,
        )
    };
    if code == 0 {
        Ok(pid as u32)
    } else {
        Err(io::Error::last_os_error())
    }
}

/// 32 random bytes as lowercase hex.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn same_token(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let mut difference = (a.len() != b.len()) as u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    difference == 0
}

/// Binds a socket in `dir` under a name no other launch has; the file is made by the bind, so a
/// name that is taken is never taken over.
fn bind_unique(dir: &Path) -> io::Result<(UnixListener, PathBuf)> {
    for _ in 0..8 {
        let socket = dir.join(format!("helper-{}.sock", &random_token()[..8]));
        match UnixListener::bind(&socket) {
            Ok(listener) => return Ok((listener, socket)),
            Err(e) if e.kind() == io::ErrorKind::AddrInUse => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AddrInUse,
        "no free socket name in the launch directory",
    ))
}

/// What bateri needs to start one helper: the socket it listens on, the key, the mach name its
/// frames arrive under.
pub struct Launch {
    listener: UnixListener,
    socket: PathBuf,
    key: String,
    mach: String,
}

impl Launch {
    /// Makes a socket in `dir` (mode `0600`), `helper-` and eight random hex digits and `.sock`;
    /// `dir` must be a directory only this user can enter (`0700`; it is created so when
    /// missing). The name is the launch's own: two launches in one directory (two helpers at
    /// once, a second one while the first is still being waited for) never share a file, and
    /// neither takes the other's away. It is short because a unix socket's whole path has to
    /// fit `sockaddr_un` (104 bytes on macOS).
    pub fn prepare(dir: &Path) -> io::Result<Self> {
        match std::fs::DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let mode = std::fs::metadata(dir)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is open to other users (mode {mode:o})", dir.display()),
            ));
        }
        let (listener, socket) = bind_unique(dir)?;
        let launch = Self {
            listener,
            socket,
            key: random_token(),
            mach: format!("{}.{}", crate::mach::NAME_PREFIX, random_token()),
        };
        // `launch` removes its socket when it goes, on any path out of here too.
        std::fs::set_permissions(&launch.socket, std::fs::Permissions::from_mode(0o600))?;
        Ok(launch)
    }

    /// The helper's command line (after the program): the whole launch contract.
    ///
    /// `root` is where profiles live and `proxy` the loopback proxy (`http://127.0.0.1:PORT`)
    /// every request of the helper goes through. The helper does not start without any of the
    /// five. It always carries `--mock-keychain`: engine 0.1.0 reads it to keep cookie encryption
    /// off the person's keychain, and newer engines use no keychain whatever it says.
    pub fn args(&self, root: &Path, proxy: &str) -> Vec<String> {
        vec![
            format!("--key={}", self.key),
            format!("--socket={}", self.socket.display()),
            format!("--mach={}", self.mach),
            format!("--root={}", root.display()),
            format!("--proxy={proxy}"),
            "--mock-keychain".into(),
        ]
    }

    /// The bootstrap name the frames are received under; register it (`mach::Receiver`) before
    /// starting the helper.
    pub fn mach_name(&self) -> &str {
        &self.mach
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    /// Waits for the helper to connect and say hello. `child` is the pid bateri started: a
    /// connection from any other process is dropped and the wait goes on. The key is spent by this
    /// call whatever happens; the socket file is removed.
    pub fn accept(self, child: u32, timeout: Duration) -> io::Result<Connection> {
        self.accept_inner(child, timeout)
    }

    fn accept_inner(&self, child: u32, timeout: Duration) -> io::Result<Connection> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + timeout;
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => match self.greet(stream, child) {
                    Ok(connection) => return Ok(connection),
                    Err(Refused::Peer) => continue,
                    Err(Refused::Hello(e)) => return Err(e),
                },
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the helper did not connect",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(e) => return Err(e),
            }
        }
    }

    fn greet(&self, stream: UnixStream, child: u32) -> Result<Connection, Refused> {
        stream.set_nonblocking(false).map_err(Refused::hello)?;
        if peer_pid(&stream).map_err(Refused::hello)? != child {
            return Err(Refused::Peer);
        }
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(Refused::hello)?;
        let mut reader = stream.try_clone().map_err(Refused::hello)?;
        let hello: Event = read_message(&mut reader, MAX_HELLO)
            .map_err(Refused::hello)?
            .ok_or_else(|| Refused::hello(io::ErrorKind::UnexpectedEof.into()))?;
        let Event::Hello {
            protocol,
            version,
            chromium,
            pid,
            key,
        } = hello
        else {
            return Err(Refused::hello(io::Error::new(
                io::ErrorKind::InvalidData,
                "the first message was not a hello",
            )));
        };
        if !same_token(&key, &self.key) {
            return Err(Refused::hello(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "wrong key",
            )));
        }
        if protocol != PROTOCOL || pid != child {
            return Err(Refused::hello(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("protocol {protocol} (this side speaks {PROTOCOL}), pid {pid}"),
            )));
        }
        stream.set_read_timeout(None).map_err(Refused::hello)?;
        let (commands, events) = halves(stream, MAX_MESSAGE).map_err(Refused::hello)?;
        Ok(Connection {
            commands,
            events,
            version,
            chromium,
            pid,
        })
    }
}

impl Drop for Launch {
    /// The key is spent and the socket is of no use to anyone else: the file goes with the launch,
    /// whether it was accepted from, timed out, or never used. It is this launch's own file.
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket);
    }
}

enum Refused {
    /// Not the helper bateri started: keep waiting.
    Peer,
    Hello(io::Error),
}

impl Refused {
    fn hello(e: io::Error) -> Self {
        Refused::Hello(e)
    }
}

/// bateri's end of a conversation with a helper that passed the handshake.
pub struct Connection {
    pub commands: Writer<Command>,
    pub events: Reader<Event>,
    /// What the hello said.
    pub version: String,
    pub chromium: String,
    pub pid: u32,
}

/// The helper's end.
pub struct Client {
    pub events: Writer<Event>,
    pub commands: Reader<Command>,
}

impl Client {
    /// Connects to `socket` and sends the hello.
    pub fn connect(socket: &Path, key: &str, version: &str, chromium: &str) -> io::Result<Self> {
        let stream = UnixStream::connect(socket)?;
        let (events, commands) = halves(stream, MAX_MESSAGE)?;
        events.send(&Event::Hello {
            protocol: PROTOCOL,
            version: version.into(),
            chromium: chromium.into(),
            pid: std::process::id(),
            key: key.into(),
        })?;
        Ok(Self { events, commands })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bf-ch-{}-{label}", std::process::id()))
    }

    #[test]
    fn two_launches_in_one_directory_keep_their_own_sockets() {
        let dir = directory("two");
        let first = Launch::prepare(&dir).unwrap();
        let second = Launch::prepare(&dir).unwrap();
        assert_ne!(first.socket(), second.socket());
        // The first one gives up (nobody connects): the end of it must not take the second's
        // socket away, which is what a shared `helper.sock` did.
        let late = first.accept(1, Duration::from_millis(50));
        assert_eq!(late.err().map(|e| e.kind()), Some(io::ErrorKind::TimedOut));
        assert!(second.socket().exists());
        UnixStream::connect(second.socket()).expect("the second launch still takes connections");
        drop(second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_launch_that_is_dropped_leaves_no_socket_and_a_stale_file_is_not_taken_over() {
        let dir = directory("drop");
        let launch = Launch::prepare(&dir).unwrap();
        let socket = launch.socket().to_path_buf();
        assert!(socket.exists());
        drop(launch);
        assert!(!socket.exists());
        // A file someone else left under a name is not this launch's to bind or remove.
        std::fs::write(dir.join("helper-aaaaaaaa.sock"), "x").unwrap();
        let other = Launch::prepare(&dir).unwrap();
        assert!(dir.join("helper-aaaaaaaa.sock").exists());
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_socket_name_is_short_and_the_key_and_mach_name_are_fresh() {
        let dir = directory("short");
        let a = Launch::prepare(&dir).unwrap();
        let b = Launch::prepare(&dir).unwrap();
        let name = a
            .socket()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(name.starts_with("helper-") && name.ends_with(".sock") && name.len() == 20);
        assert_ne!(a.key(), b.key());
        assert_ne!(a.mach_name(), b.mach_name());
        drop((a, b));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
