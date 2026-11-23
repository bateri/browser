//! A stand-in for bateri's proxy: a loopback HTTP proxy that wants credentials and answers every
//! request from a closure, so tests can serve pages, set cookies and see who asked.
//!
//! It is also the check that the helper reaches the network only through it: the helper is
//! started with no other way out.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One request that got past the credentials check.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub host: String,
    pub path: String,
    /// The proxy user the request carried.
    pub user: String,
    pub cookie: Option<String>,
    pub referer: Option<String>,
}

/// What the closure answers with.
pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Reply {
    pub fn html(body: &str) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
            body: body.as_bytes().to_vec(),
        }
    }

    pub fn status(status: u16) -> Self {
        Self {
            status,
            headers: vec![],
            body: vec![],
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// The answer promises all of its body but sends only its first `n` bytes and then stays
    /// silent (a download that is still running) until the stub proxy is dropped.
    pub fn stalled_after(self, n: usize) -> Self {
        self.header(PARTIAL, &format!("{n} stall"))
    }

    /// The answer promises all of its body, sends its first `n` bytes and cuts the connection
    /// (a download that breaks off).
    pub fn cut_after(self, n: usize) -> Self {
        self.header(PARTIAL, &format!("{n} cut"))
    }
}

/// A pseudo-header the stub reads and does not send: how much of the body goes out, and what
/// then (`stall` or `cut`).
const PARTIAL: &str = "x-stub-partial";

type Responder = dyn Fn(&Seen) -> Reply + Send + Sync;

struct Shared {
    accepted: Mutex<HashSet<(String, String)>>,
    responder: Mutex<Arc<Responder>>,
    seen: Mutex<Vec<Seen>>,
    /// Requests refused for want of (right) credentials, as `METHOD target`.
    refused: Mutex<Vec<String>>,
    /// Requests that carried a profile's identity, as `METHOD target`.
    identified: Mutex<HashSet<String>>,
    /// Host names a `CONNECT` is carried through to a local port (a test's own TLS server).
    tunnels: Mutex<HashMap<String, u16>>,
    stop: AtomicBool,
}

pub struct StubProxy {
    pub port: u16,
    shared: Arc<Shared>,
}

impl StubProxy {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the stub proxy");
        let port = listener.local_addr().unwrap().port();
        let shared = Arc::new(Shared {
            accepted: Mutex::new(HashSet::new()),
            responder: Mutex::new(Arc::new(|_| Reply::status(404))),
            seen: Mutex::new(Vec::new()),
            refused: Mutex::new(Vec::new()),
            identified: Mutex::new(HashSet::new()),
            tunnels: Mutex::new(HashMap::new()),
            stop: AtomicBool::new(false),
        });
        let accept_shared = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if accept_shared.stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(stream) = stream else { continue };
                let shared = accept_shared.clone();
                std::thread::spawn(move || serve(stream, shared));
            }
        });
        Self { port, shared }
    }

    /// Credentials the proxy accepts.
    pub fn accept(&self, user: &str, pass: &str) {
        self.shared
            .accepted
            .lock()
            .unwrap()
            .insert((user.into(), pass.into()));
    }

    /// Carries a `CONNECT` to `host` (on any port) through to `127.0.0.1:port`, so that a test's own
    /// TLS server answers an `https` address; any other `CONNECT` is turned away.
    pub fn tunnel(&self, host: &str, port: u16) {
        self.shared
            .tunnels
            .lock()
            .unwrap()
            .insert(host.to_string(), port);
    }

    pub fn respond(&self, responder: impl Fn(&Seen) -> Reply + Send + Sync + 'static) {
        *self.shared.responder.lock().unwrap() = Arc::new(responder);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.shared.seen.lock().unwrap().clone()
    }

    /// Every request turned away with a 407 (it carried no profile's identity), as
    /// `METHOD target`.
    pub fn refused(&self) -> Vec<String> {
        self.shared.refused.lock().unwrap().clone()
    }

    /// Every request that carried a profile's identity, as `METHOD target`: a `CONNECT` (an
    /// `https` address) too, which `seen` leaves out.
    pub fn identified(&self) -> Vec<String> {
        let mut all: Vec<String> = self
            .shared
            .identified
            .lock()
            .unwrap()
            .iter()
            .cloned()
            .collect();
        all.sort();
        all
    }

    /// The refused requests that never came back with a profile's identity: not the first half of
    /// a page's own request (the engine asks the proxy without credentials, hears 407, asks the
    /// browser's profile and sends it again), but one the engine made for itself, which no profile
    /// answers for.
    pub fn strays(&self) -> Vec<String> {
        let identified = self.shared.identified.lock().unwrap().clone();
        let mut strays: Vec<String> = self
            .refused()
            .into_iter()
            .filter(|r| !identified.contains(r))
            .collect();
        strays.dedup();
        strays
    }

    pub fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl Drop for StubProxy {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(stream: TcpStream, shared: Arc<Shared>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let Ok(read_half) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(read_half);
    let mut out = stream;
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return;
        };
        let (method, target) = (method.to_string(), target.to_string());
        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        // Drain a request body so the next request on the connection starts clean.
        if let Some(n) = header("content-length").and_then(|v| v.parse::<usize>().ok()) {
            let mut body = vec![0u8; n.min(1 << 20)];
            if reader.read_exact(&mut body).is_err() {
                return;
            }
        }
        let user = header("proxy-authorization")
            .and_then(|v| v.strip_prefix("Basic ").map(str::to_string))
            .and_then(|v| {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, v).ok()
            })
            .and_then(|v| String::from_utf8(v).ok())
            .and_then(|v| {
                v.split_once(':')
                    .map(|(u, p)| (u.to_string(), p.to_string()))
            })
            .filter(|cred| shared.accepted.lock().unwrap().contains(cred))
            .map(|(u, _)| u);
        let asked = format!("{method} {target}");
        let Some(user) = user else {
            shared.refused.lock().unwrap().push(asked);
            let reply = b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"stub\"\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n";
            if out.write_all(reply).is_err() {
                return;
            }
            continue;
        };
        shared.identified.lock().unwrap().insert(asked);
        if method == "CONNECT" {
            let host = target.rsplit_once(':').map_or(target.as_str(), |(h, _)| h);
            let port = shared.tunnels.lock().unwrap().get(host).copied();
            if let Some(port) = port {
                tunnel(reader, out, port);
                return;
            }
            // The other tests are plain http.
            let reply = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n";
            if out.write_all(reply).is_err() {
                return;
            }
            continue;
        }
        let rest = target
            .strip_prefix("http://")
            .unwrap_or(target.as_str())
            .to_string();
        let (host, path) = match rest.split_once('/') {
            Some((h, p)) => (h.to_string(), format!("/{p}")),
            None => (rest, "/".to_string()),
        };
        let seen = Seen {
            method,
            host,
            path,
            user,
            cookie: header("cookie"),
            referer: header("referer"),
        };
        shared.seen.lock().unwrap().push(seen.clone());
        let responder = shared.responder.lock().unwrap().clone();
        let reply = responder(&seen);
        let mut head = format!(
            "HTTP/1.1 {} X\r\nContent-Length: {}\r\n",
            reply.status,
            reply.body.len()
        );
        let mut partial = None;
        for (k, v) in &reply.headers {
            if k.eq_ignore_ascii_case(PARTIAL) {
                partial = v
                    .split_once(' ')
                    .and_then(|(n, then)| Some((n.parse::<usize>().ok()?, then.to_string())));
                continue;
            }
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        head.push_str("\r\n");
        if let Some((first, then)) = partial {
            let first = first.min(reply.body.len());
            if out.write_all(head.as_bytes()).is_err()
                || out.write_all(&reply.body[..first]).is_err()
            {
                return;
            }
            if then == "stall" {
                while !shared.stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
            return;
        }
        if out.write_all(head.as_bytes()).is_err() || out.write_all(&reply.body).is_err() {
            return;
        }
    }
}

/// Answers a `CONNECT` and carries the bytes both ways until either side closes.
fn tunnel(mut from_client: BufReader<TcpStream>, mut to_client: TcpStream, port: u16) {
    let Ok(upstream) = TcpStream::connect(("127.0.0.1", port)) else {
        let _ = to_client.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n");
        return;
    };
    if to_client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .is_err()
    {
        return;
    }
    let _ = to_client.set_read_timeout(None);
    let Ok(mut up_write) = upstream.try_clone() else {
        return;
    };
    let mut up_read = upstream;
    let closer = to_client.try_clone();
    let uploader = std::thread::spawn(move || {
        let _ = std::io::copy(&mut from_client, &mut up_write);
        let _ = up_write.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut up_read, &mut to_client);
    if let Ok(closer) = closer {
        let _ = closer.shutdown(Shutdown::Both);
    }
    let _ = uploader.join();
}
