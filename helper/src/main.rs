//! The hidden browser helper: Chromium drawing off-screen, one request context per profile, frames
//! to bateri over mach, control over a unix socket (see the `browser-ipc` crate).
//!
//! Chromium's helper processes (renderer, GPU, ...) are `bin/subprocess.rs`; this is the browser
//! process, and it never starts without the key bateri gave it.

mod agent;
mod clear;
mod commands;
mod config;
mod console;
mod convert;
mod handlers;
mod keys;
mod mac;
mod pinch;
mod records;
mod rules;
mod scroll;
mod state;
mod switches;
mod sync;

pub use state::request_quit;

use browser_ipc::{Client, Sender as FrameSender};
use cef::{args::Args, *};
use state::{Msg, Out, with};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// The most the loop sleeps without hearing from the engine: it asks for its work to be done
/// through `on_schedule_message_pump_work`, and this is only the net under that.
const HEARTBEAT: Duration = Duration::from_millis(100);
/// How often the loop turns while a window of the helper's own is open.
const WINDOW_TURN: Duration = Duration::from_millis(8);
/// How long after the last window was seen on screen the loop keeps turning that often: taking a
/// window away is also done through the event queue.
const WINDOW_GRACE: Duration = Duration::from_millis(500);
/// How long the end waits for browsers to close and cookies to be written.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(8);

wrap_app! {
    struct HelperApp {
        proxy_port: u16,
        mock_keychain: bool,
        // The file `switches::MODEL_MANIFEST` names.
        model_manifest: String,
    }

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefString>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(cl) = command_line else { return };
            // The one proxy every request goes through, loopback included (a page on localhost is
            // bateri's to see too). Chromium's own command line is off, so nothing else can set it.
            let proxy = format!("http://127.0.0.1:{}", self.proxy_port);
            cl.append_switch_with_value(
                Some(&switches::PROXY_SERVER.into()),
                Some(&proxy.as_str().into()),
            );
            if self.mock_keychain {
                cl.append_switch(Some(&switches::MOCK_KEYCHAIN.into()));
            }
            // Nothing leaves this process except through the proxy: no background services, no
            // QUIC (UDP), no host name resolved here, no WebRTC UDP outside the proxy, and none of
            // the engine's own requests (`switches` names each one and the end-to-end suite looks
            // them up in the engine).
            for switch in switches::PLAIN {
                cl.append_switch(Some(&(*switch).into()));
            }
            for (switch, value) in switches::VALUED {
                cl.append_switch_with_value(Some(&(*switch).into()), Some(&(*value).into()));
            }
            cl.append_switch_with_value(
                Some(&"disable-features".into()),
                Some(&switches::disabled_features().as_str().into()),
            );
            cl.append_switch_with_value(
                Some(&switches::MODEL_MANIFEST.into()),
                Some(&self.model_manifest.as_str().into()),
            );
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(Process::new())
        }
    }
}

wrap_browser_process_handler! {
    struct Process;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            with(|s| s.ready = true);
        }

        /// Any thread. The engine says when it wants to run next; the loop is woken for it.
        fn on_schedule_message_pump_work(&self, delay_ms: i64) {
            state::schedule_pump(delay_ms);
        }
    }
}

extern "C" fn on_signal(_: libc::c_int) {
    state::quit_from_signal();
}

/// The engine's version, from the string it was built with (`156.0.31+g...+chromium-156.0.8078.100`).
fn chromium_version() -> String {
    let full = String::from_utf8_lossy(cef_dll_sys::CEF_VERSION).into_owned();
    let full = full.trim_end_matches('\0');
    full.rsplit_once("chromium-")
        .map(|(_, v)| v)
        .unwrap_or(full)
        .to_string()
}

/// The profiles' folder: made if need be, only ours, and named without links.
fn prepare_root(path: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_dir() {
        return Err(format!("{}: not a directory", path.display()));
    }
    // SAFETY: geteuid has no preconditions.
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(format!("{}: not owned by this user", path.display()));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    path.canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn fail(message: &str, code: i32) -> ! {
    eprintln!("bateri-browser: {message}");
    std::process::exit(code);
}

fn main() {
    // Before anything is loaded: a helper nobody started with a key has nothing to do.
    let cfg = match config::parse(std::env::args().skip(1)) {
        Ok(cfg) => cfg,
        Err(e) => fail(&e.to_string(), 2),
    };
    let exe = std::env::current_exe().unwrap_or_else(|e| fail(&format!("current exe: {e}"), 1));
    let loader = library_loader::LibraryLoader::new(&exe, false);
    if !loader.load() {
        fail("the engine framework could not be loaded", 1);
    }
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    mac::setup_application();

    let root = prepare_root(&cfg.root).unwrap_or_else(|e| fail(&e, 1));
    with(|s| s.root = root.clone());
    sync::clear_stages(&root);

    // The engine takes the switch, not the file, as the sign to leave its component updater out of
    // the on-device model: a file it cannot read costs a line in its log and nothing else.
    let (name, empty) = switches::MODEL_MANIFEST_FILE;
    let model_manifest = root.join(name);
    let _ = std::fs::write(&model_manifest, empty);

    let args = Args::new();
    let mut app = HelperApp::new(
        cfg.proxy_port,
        cfg.mock_keychain,
        model_manifest.to_string_lossy().into_owned(),
    );
    let ret = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if ret >= 0 {
        std::process::exit(ret);
    }
    let settings = Settings {
        windowless_rendering_enabled: 1,
        external_message_pump: 1,
        no_sandbox: 0,
        // Chromium's own switches are off: nothing on the command line reaches the engine.
        command_line_args_disabled: 1,
        root_cache_path: root.to_string_lossy().as_ref().into(),
        // The engine loads the profile it last had a window in as its own, at its start and before
        // any request context exists, and a context made later for that folder is the engine's
        // own, with these settings and not the ones `create_context` gives. Without this, that
        // profile's preferences say "a new tab" at every start and its cookies of the session
        // are thrown away (see `create_context`, where a named profile keeps them).
        persist_session_cookies: 1,
        log_file: root.join("engine.log").to_string_lossy().as_ref().into(),
        log_severity: LogSeverity::WARNING,
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        fail("the engine did not start", 1);
    }

    // SAFETY: the handler only stores into an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }

    let (tx, rx) = mpsc::channel::<Msg>();
    let (out_tx, out_rx) = mpsc::channel::<Out>();
    state::install_channels(tx.clone(), out_tx);

    // The engine is up when its context is: only then is bateri told the helper is ready.
    while !with(|s| s.ready) {
        do_message_loop_work();
        let _ = rx.recv_timeout(Duration::from_millis(5));
    }
    match FrameSender::lookup(&cfg.mach) {
        Ok(frames) => with(|s| s.frames = Some(frames)),
        Err(e) => fail(&format!("frame port {}: {e}", cfg.mach), 1),
    }
    let client = Client::connect(
        &cfg.socket,
        &cfg.key,
        env!("CARGO_PKG_VERSION"),
        &chromium_version(),
    )
    .unwrap_or_else(|e| fail(&format!("control socket: {e}"), 1));
    let Client {
        events,
        mut commands,
    } = client;

    // bateri's commands come in on one thread, its events go out on another: neither ever waits for
    // the main thread, and the main thread never waits for the socket.
    let reader_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            match commands.recv() {
                Ok(Some(command)) => {
                    if reader_tx.send(Msg::Command(command)).is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => {
                    let _ = reader_tx.send(Msg::Hangup);
                    break;
                }
            }
        }
    });
    std::thread::spawn(move || {
        while let Ok(out) = out_rx.recv() {
            match out {
                Out::Event(event) => {
                    if events.send(&event).is_err() {
                        break;
                    }
                }
                Out::Flush(done) => {
                    let _ = done.send(());
                }
            }
        }
    });

    run(&rx);

    let flags = commands::begin_shutdown();
    let deadline = Instant::now() + SHUTDOWN_LIMIT;
    while !(commands::none_open()
        && flags
            .iter()
            .all(|f| f.load(std::sync::atomic::Ordering::SeqCst)))
        && Instant::now() < deadline
    {
        do_message_loop_work();
        // What still arrives is not acted on any more; it is read so the channel stays empty.
        while rx.try_recv().is_ok() {}
        let _ = rx.recv_timeout(Duration::from_millis(5));
    }
    commands::release_all();
    for _ in 0..20 {
        do_message_loop_work();
    }
    state::flush_output(Duration::from_secs(2));
    // The engine's own end, which the helper cannot bound: it gives itself 10 seconds from the
    // start of its teardown (then exits with 2, its "hung" code, after "Teardown watchdog expired"
    // in the log) and inside them waits up to 10 seconds for its network process to exit. On a
    // machine busy enough to starve that process's background writes, the second outlasts the
    // first. After such an end the next start still found a site's cookies (written above), Local
    // Storage, IndexedDB, service worker and Cache Storage; the profile's preferences and the
    // engine's own state were not checked.
    shutdown();
}

/// The main loop: sleeps until a command, the engine, or a dropped frame's turn comes.
fn run(rx: &mpsc::Receiver<Msg>) {
    let mut window_until: Option<Instant> = None;
    loop {
        if state::quitting() {
            return;
        }
        let now = Instant::now();
        let mut next = now + HEARTBEAT;
        // A window of the helper's own (the developer tools') wants the loop to turn often enough
        // to be drawn and typed into.
        if mac::has_windows() {
            if window_until.is_none() {
                trace!("a window of the helper's own is on screen");
            }
            window_until = Some(now + WINDOW_GRACE);
        }
        if window_until.is_some_and(|until| until > now) {
            next = now + WINDOW_TURN;
        } else if window_until.take().is_some() {
            trace!("the window of the helper's own is gone");
        }
        if let Some(at) = state::peek_pump_deadline() {
            next = next.min(at);
        }
        if let Some(at) = commands::next_repaint() {
            next = next.min(at);
        }
        if let Some(at) = sync::next_due() {
            next = next.min(at);
        }
        let first = rx.recv_timeout(next.saturating_duration_since(now));
        let mut hangup = false;
        let mut take = |msg: Msg| match msg {
            Msg::Command(command) => commands::handle(command),
            Msg::Wake => {}
            Msg::Hangup => hangup = true,
        };
        match first {
            Ok(msg) => take(msg),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(msg) = rx.try_recv() {
            take(msg);
        }
        state::take_pump_deadline();
        do_message_loop_work();
        mac::pump_events();
        commands::tick();
        if hangup {
            return;
        }
    }
}
