//! A stand-in for bateri: starts the helper the way bateri does (single-use key, socket, mach
//! name, proxy), speaks the control protocol, receives frames, and lets a test wait for what it
//! expects. The code that talks to the helper is `browser_ipc`'s own, the code bateri will run.

pub mod proxy;
pub mod tls_site;

use browser_ipc::message::*;
use browser_ipc::{Frame, Launch, Receiver, Surface, Writer};
use proxy::StubProxy;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver as ChannelReceiver, RecvTimeoutError, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_WAIT: Duration = Duration::from_secs(20);

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// What a test chooses about a helper's start.
#[derive(Default, Clone)]
pub struct Options {
    /// Reuse this profile root instead of a fresh one (to restart on the same data).
    pub root: Option<PathBuf>,
    /// Extra arguments after the contract's (a test of what the helper ignores).
    pub extra_args: Vec<String>,
}

/// The newest frame of each browser and kind, and how many came.
#[derive(Default)]
pub struct Frames {
    latest: HashMap<(u32, u32), (Frame, Surface)>,
    count: HashMap<(u32, u32), u32>,
    sizes: Vec<(u32, u32, u32)>,
}

#[derive(Clone, Default)]
pub struct FrameLog(Arc<Mutex<Frames>>);

impl FrameLog {
    pub fn count(&self, browser: u32, kind: u32) -> u32 {
        *self
            .0
            .lock()
            .unwrap()
            .count
            .get(&(browser, kind))
            .unwrap_or(&0)
    }

    pub fn latest(&self, browser: u32, kind: u32) -> Option<(Frame, Surface)> {
        self.0.lock().unwrap().latest.get(&(browser, kind)).cloned()
    }

    /// The BGRA pixels of the newest frame at `points`.
    pub fn pixels(&self, browser: u32, points: &[(usize, usize)]) -> Option<(Frame, Vec<[u8; 4]>)> {
        let (frame, surface) = self.latest(browser, KIND_VIEW)?;
        Some((frame, surface.read(points)))
    }

    /// Sizes (browser, width, height) of every view frame so far, in order.
    pub fn sizes(&self) -> Vec<(u32, u32, u32)> {
        self.0.lock().unwrap().sizes.clone()
    }
}

use browser_ipc::{KIND_POPUP, KIND_VIEW};

pub struct Fake {
    pub dir: PathBuf,
    pub root: PathBuf,
    pub proxy: StubProxy,
    pub frames: FrameLog,
    pub version: String,
    pub chromium: String,
    pub pid: u32,
    commands: Writer<Command>,
    events: ChannelReceiver<Event>,
    history: Vec<Event>,
    cursor: usize,
    child: Option<Child>,
    stop_frames: Arc<AtomicBool>,
    pause_frames: Arc<AtomicBool>,
    next_req: u32,
}

/// The path of the app under test: `BROWSER_APP` names the bundle `make app` made.
pub fn app_executable() -> PathBuf {
    let app = std::env::var_os("BROWSER_APP")
        .map(PathBuf::from)
        .expect("BROWSER_APP is not set: run these through `make check-heavy`");
    let name = app
        .file_stem()
        .expect("app name")
        .to_string_lossy()
        .into_owned();
    app.join("Contents/MacOS").join(name)
}

/// A scratch directory of this test run's own.
pub fn scratch(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let dir = std::env::temp_dir().join(format!("bf-{}-{n}-{label}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

impl Fake {
    pub fn start() -> Self {
        Self::start_with(Options::default())
    }

    pub fn start_with(options: Options) -> Self {
        let dir = scratch("fake");
        let proxy = StubProxy::start();
        let root = options.root.clone().unwrap_or_else(|| dir.join("root"));
        let launch = Launch::prepare(&dir.join("sock")).expect("prepare the launch");
        let receiver = Receiver::register(launch.mach_name()).expect("register the mach name");
        let mut args = launch.args(&root, &proxy.url(), true);
        args.extend(options.extra_args.clone());
        let log = std::fs::File::create(dir.join("helper.log")).expect("log file");
        let child = std::process::Command::new(app_executable())
            .args(&args)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .expect("start the helper");
        let pid = child.id();
        let mut connection = match launch.accept(pid, Duration::from_secs(30)) {
            Ok(c) => c,
            Err(e) => panic!(
                "the helper did not shake hands: {e}\n{}",
                tail(&dir.join("helper.log"))
            ),
        };
        // Frames arrive on their own thread: a test waiting on events must not stall the helper.
        let frames = FrameLog::default();
        let stop_frames = Arc::new(AtomicBool::new(false));
        let pause_frames = Arc::new(AtomicBool::new(false));
        {
            let (frames, stop, pause) =
                (frames.0.clone(), stop_frames.clone(), pause_frames.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    if pause.load(Ordering::SeqCst) {
                        std::thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    match receiver.recv(50) {
                        Ok(Some((frame, surface))) => {
                            let mut f = frames.lock().unwrap();
                            let key = (frame.browser, frame.kind);
                            *f.count.entry(key).or_default() += 1;
                            if frame.kind == KIND_VIEW {
                                f.sizes.push((frame.browser, frame.width, frame.height));
                            }
                            f.latest.insert(key, (frame, surface));
                        }
                        Ok(None) => {}
                        Err(_) => break,
                    }
                }
            });
        }
        let (tx, rx) = channel();
        let mut events = connection.events;
        std::thread::spawn(move || {
            while let Ok(Some(event)) = events.recv() {
                if tx.send(event).is_err() {
                    break;
                }
            }
        });
        Self {
            dir,
            root,
            proxy,
            frames,
            version: connection.version.clone(),
            chromium: std::mem::take(&mut connection.chromium),
            pid,
            commands: connection.commands,
            events: rx,
            history: Vec::new(),
            cursor: 0,
            child: Some(child),
            stop_frames,
            pause_frames,
            next_req: 1,
        }
    }

    pub fn log(&self) -> String {
        tail(&self.dir.join("helper.log"))
    }

    pub fn send(&self, command: Command) {
        self.commands.send(&command).expect("send a command");
    }

    /// A fresh request number.
    pub fn req(&mut self) -> u32 {
        self.next_req += 1;
        self.next_req
    }

    fn pull(&mut self, timeout: Duration) -> bool {
        match self.events.recv_timeout(timeout) {
            Ok(event) => {
                self.history.push(event);
                true
            }
            Err(RecvTimeoutError::Timeout) => false,
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(timeout.min(Duration::from_millis(50)));
                false
            }
        }
    }

    /// Waits for the first event from here on that `pick` takes; events before it are passed over
    /// (they stay in `history`). Panics with the recent events when nothing matches in time.
    pub fn wait<T>(&mut self, within: Duration, mut pick: impl FnMut(&Event) -> Option<T>) -> T {
        let deadline = Instant::now() + within;
        loop {
            while self.cursor < self.history.len() {
                let i = self.cursor;
                self.cursor += 1;
                if let Some(found) = pick(&self.history[i]) {
                    return found;
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero()
                || !self.pull(left.min(Duration::from_millis(100))) && Instant::now() >= deadline
            {
                panic!(
                    "no matching event within {within:?}; the last events:\n{}\nhelper log:\n{}",
                    self.recent(12),
                    self.log()
                );
            }
        }
    }

    /// Like `wait`, but gives up quietly: `None` when nothing matched in time.
    pub fn try_wait<T>(
        &mut self,
        within: Duration,
        mut pick: impl FnMut(&Event) -> Option<T>,
    ) -> Option<T> {
        let deadline = Instant::now() + within;
        loop {
            while self.cursor < self.history.len() {
                let i = self.cursor;
                self.cursor += 1;
                if let Some(found) = pick(&self.history[i]) {
                    return Some(found);
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.pull(left.min(Duration::from_millis(100)));
        }
    }

    pub fn history(&self) -> &[Event] {
        &self.history
    }

    /// Skips what has arrived so far: later waits look only at what comes next.
    pub fn forget(&mut self) {
        while self.pull(Duration::from_millis(1)) {}
        self.cursor = self.history.len();
    }

    pub fn recent(&self, n: usize) -> String {
        let from = self.history.len().saturating_sub(n);
        self.history[from..]
            .iter()
            .map(|e| format!("  {e:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Opens browser `id` in a profile and waits until it exists.
    pub fn open(&mut self, id: u32, profile: Profile, width: u32, height: u32, scale: f32) {
        self.proxy.accept(&profile.user, &profile.pass);
        self.send(Command::Open {
            browser: id,
            profile,
            width,
            height,
            scale,
        });
        self.wait(DEFAULT_WAIT, |e| {
            matches!(e, Event::Opened { browser } if *browser == id).then_some(())
        });
        // The blank page the browser starts on is done loading: a later `go` is not mistaken for it.
        self.state_where(id, |s| s.url == "about:blank" && !s.loading);
    }

    /// Goes to `url` and waits until the load ends, and a moment more: keys typed in the first
    /// ~200 ms of a page that has no form control never reach its scripts (they do come back
    /// as `UnhandledKey`), so a test that types right after `go` would be racing the engine.
    pub fn go(&mut self, id: u32, url: &str) -> State {
        self.send(Command::Navigate {
            browser: id,
            url: url.into(),
        });
        let state = self.settled(id);
        std::thread::sleep(Duration::from_millis(300));
        state
    }

    /// The next state of `id` that is not loading, after it has been loading or at once when the
    /// page is already settled.
    pub fn settled(&mut self, id: u32) -> State {
        let mut loading_seen = false;
        self.wait(DEFAULT_WAIT, |e| match State::of(e, id) {
            Some(s) if s.loading => {
                loading_seen = true;
                None
            }
            Some(s) if loading_seen => Some(s),
            _ => None,
        })
    }

    /// Waits until the state of `id` satisfies `ok`.
    pub fn state_where(&mut self, id: u32, ok: impl Fn(&State) -> bool) -> State {
        // The state it is in now counts: a wait that consumed events (`agent`, `wait`) may have
        // passed the very event this one is looking for.
        if let Some(now) = self.history.iter().rev().find_map(|e| State::of(e, id))
            && ok(&now)
        {
            return now;
        }
        self.wait(DEFAULT_WAIT, |e| State::of(e, id).filter(|s| ok(s)))
    }

    /// Waits until the page's title is `title` (tests report page events through it).
    pub fn title_is(&mut self, id: u32, title: &str) -> State {
        let want = title.to_string();
        self.state_where(id, move |s| s.title == want)
    }

    pub fn mouse(&self, id: u32, kind: MouseKind, x: i32, y: i32, button: Button) {
        self.send(Command::Mouse {
            browser: id,
            kind,
            x,
            y,
            button,
            clicks: 1,
            mods: 0,
        });
    }

    pub fn click(&self, id: u32, x: i32, y: i32) {
        self.mouse(id, MouseKind::Move, x, y, Button::Left);
        self.mouse(id, MouseKind::Down, x, y, Button::Left);
        self.mouse(id, MouseKind::Up, x, y, Button::Left);
    }

    /// Runs an agent operation and waits for its answer.
    pub fn agent(&mut self, id: u32, op: AgentOp) -> AgentOutput {
        let req = self.req();
        self.send(Command::Agent {
            browser: id,
            req,
            op,
        });
        self.wait(DEFAULT_WAIT, |e| match e {
            Event::AgentResult { req: r, output, .. } if *r == req => Some(output.clone()),
            _ => None,
        })
    }

    /// Waits until the newest view frame has a pixel at `at` that `ok` accepts.
    pub fn pixel_where(
        &mut self,
        id: u32,
        at: (usize, usize),
        ok: impl Fn([u8; 4]) -> bool,
    ) -> [u8; 4] {
        let deadline = Instant::now() + DEFAULT_WAIT;
        loop {
            if let Some((_, px)) = self.frames.pixels(id, &[at])
                && ok(px[0])
            {
                return px[0];
            }
            if Instant::now() >= deadline {
                let last = self.frames.pixels(id, &[at]);
                panic!(
                    "pixel {at:?} never became right; last {last:?}; frames {}",
                    self.frames.count(id, KIND_VIEW)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub fn popup_frames(&self, id: u32) -> u32 {
        self.frames.count(id, KIND_POPUP)
    }

    /// Empties the HTTP cache and `origin`'s service workers and Cache Storage; waits for the answer
    /// (its error).
    pub fn clear_cache(&mut self, id: u32, origin: &str) -> Option<String> {
        let req = self.req();
        self.send(Command::ClearCache {
            browser: id,
            req,
            origin: origin.into(),
        });
        self.wait(DEFAULT_WAIT, |e| match e {
            Event::CacheCleared { req: r, error, .. } if *r == req => Some(error.clone()),
            _ => None,
        })
    }

    /// Deletes what the profile keeps for `host`; waits for the answer (what was there, and its
    /// error).
    pub fn clear_site(&mut self, id: u32, host: &str) -> (Vec<Record>, Option<String>) {
        let req = self.req();
        self.send(Command::ClearSiteData {
            browser: id,
            req,
            host: host.into(),
        });
        self.wait(DEFAULT_WAIT, |e| match e {
            Event::SiteCleared {
                req: r,
                records,
                error,
                ..
            } if *r == req => Some((records.clone(), error.clone())),
            _ => None,
        })
    }

    /// Reads what the browser's profile holds (cookies, Local Storage of the open pages).
    pub fn profile_read(&mut self, id: u32) -> Vec<Record> {
        let req = self.req();
        self.send(Command::ProfileRead { browser: id, req });
        let (records, error) = self.wait(DEFAULT_WAIT, |e| match e {
            Event::ProfileRecords {
                req: r,
                records,
                error,
                ..
            } if *r == req => Some((records.clone(), error.clone())),
            _ => None,
        });
        assert_eq!(error, None, "reading the profile");
        records
    }

    /// Writes records into the browser's profile; returns how many are waiting for the first page
    /// of their origin.
    pub fn profile_write(&mut self, id: u32, records: Vec<Record>) -> u32 {
        let req = self.req();
        self.send(Command::ProfileWrite {
            browser: id,
            req,
            records,
        });
        let (deferred, error) = self.wait(DEFAULT_WAIT, |e| match e {
            Event::ProfileWritten {
                req: r,
                deferred,
                error,
                ..
            } if *r == req => Some((*deferred, error.clone())),
            _ => None,
        });
        assert_eq!(error, None, "writing the profile");
        deferred
    }

    pub fn profile_watch(&self, id: u32, on: bool) {
        self.send(Command::ProfileWatch { browser: id, on });
    }

    /// A place in the events so far: `changed_since` looks at what comes after it, whatever a
    /// wait in between has passed over.
    pub fn mark(&mut self) -> usize {
        while self.pull(Duration::from_millis(1)) {}
        self.history.len()
    }

    /// Waits for a report of changes after `mark` that holds a record `pick` takes; returns all of
    /// that report's records and its stage.
    pub fn changed_since(
        &mut self,
        mark: usize,
        within: Duration,
        pick: impl Fn(&Record) -> bool,
    ) -> Option<(Vec<Record>, Option<String>)> {
        let deadline = Instant::now() + within;
        let mut at = mark;
        loop {
            while at < self.history.len() {
                if let Event::ProfileChanged { records, stage, .. } = &self.history[at]
                    && records.iter().any(&pick)
                {
                    return Some((records.clone(), stage.clone()));
                }
                at += 1;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.pull(left.min(Duration::from_millis(100)));
        }
    }

    /// Copies the IndexedDB folders of the profile in `dir` into a stage.
    pub fn indexed_db_read(&mut self, dir: &str) -> (Vec<Record>, String) {
        let req = self.req();
        self.send(Command::IndexedDbRead {
            req,
            dir: dir.into(),
        });
        let (records, stage, error) = self.wait(DEFAULT_WAIT, |e| match e {
            Event::IndexedDbRecords {
                req: r,
                records,
                stage,
                error,
            } if *r == req => Some((records.clone(), stage.clone(), error.clone())),
            _ => None,
        });
        assert_eq!(error, None, "reading the folders");
        (records, stage)
    }

    /// Puts IndexedDB records from a stage into the profile in `dir`: (written, deferred).
    pub fn indexed_db_write(&mut self, dir: &str, stage: &str, records: Vec<Record>) -> (u32, u32) {
        let req = self.req();
        self.send(Command::IndexedDbWrite {
            req,
            dir: dir.into(),
            stage: stage.into(),
            records,
        });
        let (written, deferred, error) = self.wait(DEFAULT_WAIT, |e| match e {
            Event::IndexedDbWritten {
                req: r,
                written,
                deferred,
                error,
            } if *r == req => Some((*written, *deferred, error.clone())),
            _ => None,
        });
        assert_eq!(error, None, "writing the folders");
        (written, deferred)
    }

    /// Asks the helper to quit and waits for it; returns the exit code. Any other end than 0 puts
    /// the helper's last lines in the test's output: an exit of 2 with "Teardown watchdog
    /// expired" is the engine giving up on its own shutdown (on a machine too busy to let its
    /// network process finish in 10 seconds), not the helper's.
    pub fn quit(&mut self) -> Option<i32> {
        let _ = self.commands.send(&Command::Quit);
        let code = self.reap(Duration::from_secs(15));
        if code != Some(0) {
            eprintln!("the helper ended with {code:?}; its log:\n{}", self.log());
        }
        code
    }

    /// Stops (or resumes) taking frames off the port: a receiver that is behind.
    pub fn pause_frames(&self, on: bool) {
        self.pause_frames.store(on, Ordering::SeqCst);
    }

    /// The helper's own child processes (renderer, GPU, network): `(pid, type)`.
    pub fn subprocesses(&self) -> Vec<(u32, String)> {
        self.children()
            .into_iter()
            .map(|(pid, command)| {
                let kind = command
                    .split_whitespace()
                    .find_map(|a| a.strip_prefix("--type="))
                    .unwrap_or("")
                    .to_string();
                (pid, kind)
            })
            .collect()
    }

    /// The windows the helper owns, as the window server lists them (no permission is needed for
    /// who owns a window and whether it is on screen).
    pub fn windows(&self) -> Vec<WindowFact> {
        windows_of(self.pid)
    }

    /// The helper's own child processes with their whole command lines.
    pub fn children(&self) -> Vec<(u32, String)> {
        let out = std::process::Command::new("ps")
            .args(["-axo", "pid=,ppid=,command="])
            .output()
            .expect("ps");
        let text = String::from_utf8_lossy(&out.stdout);
        let mut found = Vec::new();
        for line in text.lines() {
            let Some((pid, rest)) = line.trim_start().split_once(char::is_whitespace) else {
                continue;
            };
            let rest = rest.trim_start();
            let (ppid, command) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            if ppid.parse::<u32>().ok() != Some(self.pid) {
                continue;
            }
            found.push((pid.parse().unwrap(), command.trim().to_string()));
        }
        found
    }

    fn reap(&mut self, within: Duration) -> Option<i32> {
        let mut child = self.child.take()?;
        let deadline = Instant::now() + within;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => return status.code(),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20))
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
    }

    /// Waits for the helper to exit by itself (after its socket closed, say).
    pub fn exit_code(&mut self, within: Duration) -> Option<i32> {
        self.reap(within)
    }

    /// Ends the conversation from bateri's side without a `Quit`.
    pub fn hang_up(&self) {
        self.commands.shutdown();
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        self.stop_frames.store(true, Ordering::SeqCst);
        if self.child.is_some() {
            let _ = self.commands.send(&Command::Quit);
            self.reap(Duration::from_secs(10));
        }
        // A profile root given by the test lives elsewhere and is the test's to remove.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// The browser state events carry, the way a page would hold it.
pub type State = StateView;

#[derive(Clone, Debug, PartialEq)]
pub struct StateView {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    pub status: String,
    pub zoom: f64,
    pub error: Option<LoadError>,
    pub found: Option<Found>,
}

impl StateView {
    pub fn of(event: &Event, id: u32) -> Option<Self> {
        match event {
            Event::State {
                browser,
                url,
                title,
                loading,
                can_back,
                can_forward,
                status,
                zoom,
                error,
                found,
            } if *browser == id => Some(Self {
                url: url.clone(),
                title: title.clone(),
                loading: *loading,
                can_back: *can_back,
                can_forward: *can_forward,
                status: status.clone(),
                zoom: *zoom,
                error: error.clone(),
                found: *found,
            }),
            _ => None,
        }
    }
}

fn tail(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(40)..].join("\n")
}

/// A profile for tests: persisted under `dir`, or in memory when `dir` is `None`.
pub fn profile(context: &str, dir: Option<&str>) -> Profile {
    Profile {
        context: context.into(),
        dir: dir.map(str::to_string),
        user: format!("user-{context}"),
        pass: format!("pass-{context}"),
    }
}

/// An RGB the page shows, as the BGRA a frame holds.
pub fn bgra(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 255]
}

/// One window of a process: its layer (0 is an ordinary window) and whether it is on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowFact {
    pub layer: i32,
    pub on_screen: bool,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> *const std::ffi::c_void;
    static kCGWindowOwnerPID: *const std::ffi::c_void;
    static kCGWindowLayer: *const std::ffi::c_void;
    static kCGWindowIsOnscreen: *const std::ffi::c_void;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFArrayGetCount(array: *const std::ffi::c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const std::ffi::c_void, at: isize)
    -> *const std::ffi::c_void;
    fn CFDictionaryGetValue(
        dict: *const std::ffi::c_void,
        key: *const std::ffi::c_void,
    ) -> *const std::ffi::c_void;
    fn CFNumberGetValue(number: *const std::ffi::c_void, kind: i32, out: *mut i32) -> bool;
    fn CFBooleanGetValue(boolean: *const std::ffi::c_void) -> bool;
    fn CFRelease(object: *const std::ffi::c_void);
}

fn windows_of(pid: u32) -> Vec<WindowFact> {
    const ALL_WINDOWS: u32 = 0;
    const NO_WINDOW: u32 = 0;
    const SINT32: i32 = 3;
    let mut found = Vec::new();
    unsafe {
        let list = CGWindowListCopyWindowInfo(ALL_WINDOWS, NO_WINDOW);
        if list.is_null() {
            return found;
        }
        for at in 0..CFArrayGetCount(list) {
            let entry = CFArrayGetValueAtIndex(list, at);
            let number = |key: *const std::ffi::c_void| {
                let value = CFDictionaryGetValue(entry, key);
                let mut out = 0i32;
                (!value.is_null() && CFNumberGetValue(value, SINT32, &mut out)).then_some(out)
            };
            if number(kCGWindowOwnerPID) != Some(pid as i32) {
                continue;
            }
            let shown = CFDictionaryGetValue(entry, kCGWindowIsOnscreen);
            found.push(WindowFact {
                layer: number(kCGWindowLayer).unwrap_or(0),
                on_screen: !shown.is_null() && CFBooleanGetValue(shown),
            });
        }
        CFRelease(list);
    }
    found
}
