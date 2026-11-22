//! The helper end to end: the real app bundle, started the way bateri starts it, driven over the
//! control protocol, its frames received over mach. Every test is `#[ignore]`d: they need the
//! bundle, so they run through `make check-heavy`.
//!
//! Pages come from the stub proxy under made-up host names; the helper reaches nothing else.
//! Page events are read back through `document.title`.

use base64::Engine;
use browser_fake::proxy::{Reply, Seen};
use browser_fake::{Fake, Options, StateView, app_executable, bgra, profile, scratch};
use browser_ipc::message::*;
use browser_ipc::{KIND_VIEW, import};
use std::time::{Duration, Instant};

/// The helper's own list of what it puts on the engine's command line and in its preferences.
#[path = "../../helper/src/switches.rs"]
#[allow(dead_code)]
mod switches;

const SHORT: Duration = Duration::from_secs(3);
const KEY_ESCAPE: i32 = 27;

fn serve(fake: &Fake, html: &'static str) {
    fake.proxy.respond(move |_| Reply::html(html));
}

/// Polls `ok` (for things only the test can see: frames, files, processes) for up to 15 seconds.
fn wait_until(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !ok() {
        assert!(Instant::now() < deadline, "never happened: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn key(fake: &Fake, id: u32, kind: KeyKind, vk: i32, native: i32, ch: u16, mods: u32) {
    fake.send(Command::Key {
        browser: id,
        kind,
        key: KeyData {
            vk,
            native,
            character: ch,
            unmodified: ch,
            mods,
            system: false,
        },
        commands: vec![],
    });
}

/// One physical key as the system reports it: a key down, and a key up.
fn tap(fake: &Fake, id: u32, vk: i32, native: i32, ch: u16, mods: u32) {
    key(fake, id, KeyKind::Down, vk, native, ch, mods);
    key(fake, id, KeyKind::Up, vk, native, ch, mods);
}

/// A printable character: down, the character, up.
fn type_char(fake: &Fake, id: u32, ch: char) {
    let vk = ch.to_ascii_uppercase() as i32;
    key(fake, id, KeyKind::Down, vk, 0, ch as u16, 0);
    key(fake, id, KeyKind::Char, ch as i32, 0, ch as u16, 0);
    key(fake, id, KeyKind::Up, vk, 0, ch as u16, 0);
}

fn right_click(fake: &Fake, id: u32, x: i32, y: i32) {
    fake.mouse(id, MouseKind::Move, x, y, Button::Right);
    fake.mouse(id, MouseKind::Down, x, y, Button::Right);
    fake.mouse(id, MouseKind::Up, x, y, Button::Right);
}

fn requests_for(fake: &Fake, path: &str) -> Vec<Seen> {
    fake.proxy
        .seen()
        .into_iter()
        .filter(|s| s.path == path)
        .collect()
}

// ---- start and paint ----

#[test]
#[ignore]
fn a_page_loads_and_paints() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>hello</title><body style='margin:0;background:#c80000'>",
    );
    fake.open(1, profile("a", None), 400, 300, 2.0);
    let state = fake.go(1, "http://page.test/");
    assert_eq!(state.title, "hello");
    assert_eq!(state.url, "http://page.test/");
    fake.pixel_where(1, (10, 10), |px| px == bgra(200, 0, 0));
    // Frames are in device pixels: points times the scale.
    assert!(
        fake.frames
            .sizes()
            .iter()
            .any(|&(b, w, h)| b == 1 && w == 800 && h == 600)
    );
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn a_data_page_paints() {
    let mut fake = Fake::start();
    fake.open(1, profile("a", None), 200, 100, 1.0);
    // No spaces: an address with one is refused (`only_the_closed_set_of_schemes_loads`).
    let url = "data:text/html,<title>d</title><body%20style='margin:0;background:%2300c800'>";
    let state = fake.go(1, url);
    assert_eq!(state.title, "d");
    assert_eq!(state.url, url);
    fake.pixel_where(1, (5, 5), |px| px == bgra(0, 200, 0));
    // The page is the helper's own: nothing was asked of the network.
    assert!(fake.proxy.seen().is_empty());
}

#[test]
#[ignore]
fn frames_follow_the_view_size_and_scale() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0;background:#c80000'>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.pixel_where(1, (10, 10), |px| px == bgra(200, 0, 0));
    fake.send(Command::Resize {
        browser: 1,
        width: 300,
        height: 200,
        scale: 2.0,
    });
    wait_until("a 600x400 frame", || {
        fake.frames
            .latest(1, KIND_VIEW)
            .is_some_and(|(f, _)| (f.width, f.height) == (600, 400))
    });
    fake.pixel_where(1, (300, 200), |px| px == bgra(200, 0, 0));
}

#[test]
#[ignore]
fn a_frame_imports_into_a_gpu_texture() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0;background:#c80000'>",
    );
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    fake.pixel_where(1, (10, 10), |px| px == bgra(200, 0, 0));
    let (_, surface) = fake.frames.latest(1, KIND_VIEW).expect("a frame");
    let instance = browser_ipc::wgpu::Instance::new(
        browser_ipc::wgpu::InstanceDescriptor::new_without_display_handle(),
    );
    let adapter =
        pollster::block_on(instance.request_adapter(&Default::default())).expect("an adapter");
    let (device, queue) =
        pollster::block_on(adapter.request_device(&Default::default())).expect("a device");
    let texture = import::copy(&device, &queue, &surface).expect("the copy");
    let px = import::read_back(&device, &queue, &texture, &[(10, 10)]);
    assert_eq!(px[0], bgra(200, 0, 0));
}

#[test]
#[ignore]
fn a_receiver_that_is_behind_loses_frames_not_the_helper() {
    let mut fake = Fake::start();
    fake.proxy.respond(|seen| {
        if seen.path == "/b" {
            Reply::html("<title>b</title><body style='margin:0;background:#00c800'>")
        } else {
            // Repaints every few milliseconds, so frames keep coming.
            Reply::html(
                "<title>a</title><body style='margin:0;background:#c80000'><script>let n=0;\
                 setInterval(()=>{document.body.style.background=(++n%2)?'#c80000':'#c80001'},8)</script>",
            )
        }
    });
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    fake.pixel_where(1, (10, 10), |px| px[2] == 200 && px[1] == 0);
    fake.pause_frames(true);
    std::thread::sleep(Duration::from_millis(1200));
    // The receiver is not looking: the helper still answers, and does not queue pictures for it.
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://page.test/b".into(),
    });
    fake.title_is(1, "b");
    fake.pause_frames(false);
    // What it was not given is not lost for good: the newest picture comes when there is room.
    fake.pixel_where(1, (10, 10), |px| px == bgra(0, 200, 0));
}

// ---- starting ----

#[test]
#[ignore]
fn it_does_not_start_without_a_key_and_the_rest_of_the_contract() {
    let exe = app_executable();
    let run = |args: &[&str]| {
        std::process::Command::new(&exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("run the helper")
    };
    let nothing = run(&[]);
    assert_eq!(nothing.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&nothing.stderr).contains("no key"));
    let no_socket = run(&["--key=abc"]);
    assert_eq!(no_socket.status.code(), Some(2));
    let other_proxy = run(&[
        "--key=abc",
        "--socket=/nonexistent",
        "--mach=x",
        "--root=/tmp/x",
        "--proxy=http://example.com:8080",
    ]);
    assert_eq!(other_proxy.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&other_proxy.stderr).contains("127.0.0.1"));
}

#[test]
#[ignore]
fn the_helper_says_who_it_is() {
    let mut fake = Fake::start();
    assert_eq!(fake.version, env!("CARGO_PKG_VERSION"));
    assert!(
        fake.chromium.starts_with("156."),
        "chromium {}",
        fake.chromium
    );
    assert!(fake.pid > 0);
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn the_renderer_stays_caged_even_when_asked_not_to() {
    unsafe extern "C" {
        fn sandbox_check(pid: i32, operation: *const std::ffi::c_char, filter: i32, ...) -> i32;
    }
    // 1: the sandbox profile of that process denies it. A process with no sandbox allows everything.
    let denies_reading_home = |pid: u32| {
        let home = std::env::var("HOME").expect("HOME");
        let path = std::ffi::CString::new(format!("{home}/.zshrc")).unwrap();
        let op = std::ffi::CString::new("file-read-data").unwrap();
        // SANDBOX_FILTER_PATH | SANDBOX_CHECK_NO_REPORT
        unsafe { sandbox_check(pid as i32, op.as_ptr(), 1 | 0x4000_0000, path.as_ptr()) == 1 }
    };
    let mut fake = Fake::start_with(Options {
        extra_args: vec!["--no-sandbox".into()],
        ..Default::default()
    });
    serve(&fake, "<title>t</title>");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    let children = fake.children();
    assert!(!children.is_empty());
    assert!(
        children
            .iter()
            .all(|(_, command)| !command.contains("--no-sandbox")),
        "a child process was started without its cage: {children:?}"
    );
    let renderers: Vec<u32> = fake
        .subprocesses()
        .into_iter()
        .filter(|(_, kind)| kind == "renderer")
        .map(|(pid, _)| pid)
        .collect();
    assert!(
        !renderers.is_empty(),
        "no renderer: {:?}",
        fake.subprocesses()
    );
    for pid in renderers {
        assert!(
            denies_reading_home(pid),
            "renderer {pid} can read the home folder"
        );
    }
    // The browser process is the one that reads and writes profiles: it is not caged.
    assert!(!denies_reading_home(fake.pid));
}

// ---- the network ----

#[test]
#[ignore]
fn every_request_goes_through_the_proxy_with_the_profiles_own_identity() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title>");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.open(2, profile("b", None), 200, 100, 1.0);
    // Loopback is not exempt: a page on localhost is bateri's to see too.
    fake.go(1, "http://localhost:8123/one");
    fake.go(2, "http://localhost:8123/two");
    let one = requests_for(&fake, "/one");
    let two = requests_for(&fake, "/two");
    assert_eq!(one.len(), 1, "{:?}", fake.proxy.seen());
    assert_eq!(one[0].user, "user-a");
    assert_eq!(two[0].user, "user-b");
    assert!(one[0].host.starts_with("localhost"), "{:?}", one[0]);
}

#[test]
#[ignore]
fn a_wrong_identity_gets_the_proxys_refusal_and_no_loop() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title>");
    // No `accept` for this profile: the proxy answers 407 to whatever it sends.
    fake.send(Command::Open {
        browser: 1,
        profile: profile("a", None),
        width: 200,
        height: 100,
        scale: 1.0,
    });
    fake.wait(browser_fake::DEFAULT_WAIT, |e| {
        matches!(e, Event::Opened { .. }).then_some(())
    });
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://page.test/".into(),
    });
    let failed = fake.state_where(1, |s| s.error.is_some());
    assert!(
        fake.proxy.seen().is_empty(),
        "a page was served to an unknown identity"
    );
    let refused = fake.proxy.refused().len();
    assert!((1..=40).contains(&refused), "asked {refused} times");
    assert!(failed.error.is_some());
    // Still alive and answering.
    fake.send(Command::Stop { browser: 1 });
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn only_the_closed_set_of_schemes_loads() {
    let mut fake = Fake::start();
    fake.proxy.respond(|seen| match seen.path.as_str() {
        "/redirect" => Reply::status(302).header("Location", "file:///etc/hosts"),
        "/frames" => Reply::html(
            "<title>frames</title><iframe src='file:///etc/hosts'></iframe>\
             <iframe srcdoc=\"<script>parent.document.title+=':srcdoc'</script>\"></iframe>\
             <script>const u=URL.createObjectURL(new Blob([\"<script>parent.document.title+=':blob'<\\/script>\"],{type:'text/html'}));\
             const f=document.createElement('iframe');f.src=u;document.body.appendChild(f)</script>",
        ),
        _ => Reply::html("<title>t</title>"),
    });
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    for (url, reason) in [
        ("file:///etc/hosts", "scheme"),
        ("javascript:alert(1)", "scheme"),
        ("ftp://example.com/", "scheme"),
        ("chrome://version", "scheme"),
        ("blob:http://page.test/abc", "scheme"),
        ("about:srcdoc", "scheme"),
        ("http://page.test/a b", "address"),
        ("http://page.test/\nx", "address"),
    ] {
        fake.send(Command::Navigate {
            browser: 1,
            url: url.into(),
        });
        let (got, why) = fake.wait(SHORT, |e| match e {
            Event::Refused { url, reason, .. } => Some((url.clone(), reason.clone())),
            _ => None,
        });
        assert_eq!(why, reason, "{url:?} -> {got:?}");
    }
    // A redirect out of the set never loads: Chromium itself stops redirects to local schemes
    // (`ERR_UNSAFE_REDIRECT`) before the helper's `OnBeforeBrowse` would see them.
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://page.test/redirect".into(),
    });
    fake.wait(SHORT, |e| match e {
        Event::Refused { url, .. } if url == "file:///etc/hosts" => Some(()),
        Event::State {
            error: Some(LoadError { code: -311, .. }),
            ..
        } => Some(()),
        _ => None,
    });
    // `about:srcdoc` and `blob:` frames are allowed in a page, and work; a frame on a local file
    // does not (nothing to see: the page's own title says what loaded).
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://page.test/frames".into(),
    });
    fake.title_is(1, "frames:srcdoc:blob");
}

// ---- input ----

#[test]
#[ignore]
fn clicks_text_wheel_and_cursor_reach_the_page() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <button style='position:absolute;left:10px;top:10px;width:100px;height:40px;cursor:pointer' \
         onclick=\"document.title='clicked'\">go</button>\
         <input id=i style='position:absolute;left:10px;top:100px;width:200px;height:30px' \
         oninput=\"document.title='in:'+this.value\">\
         <div style='position:absolute;top:0;height:4000px;width:1px'></div>\
         <script>addEventListener('scroll',()=>{document.title='y:'+Math.round(scrollY)})</script>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });

    fake.forget();
    fake.mouse(1, MouseKind::Move, 50, 30, Button::Left);
    fake.wait(SHORT, |e| {
        matches!(
            e,
            Event::Cursor {
                cursor: Cursor::Hand,
                ..
            }
        )
        .then_some(())
    });
    fake.click(1, 50, 30);
    fake.title_is(1, "clicked");

    fake.click(1, 100, 115);
    // Text the way the input method hands it over: whole, a letter with no key of its own.
    fake.send(Command::ImeCommit {
        browser: 1,
        text: "çay".into(),
    });
    fake.title_is(1, "in:çay");
    type_char(&fake, 1, 'x');
    fake.title_is(1, "in:çayx");

    fake.send(Command::Wheel {
        browser: 1,
        x: 300,
        y: 200,
        dx: 0,
        dy: -300,
        mods: 0,
    });
    fake.state_where(1, |s| s.title.starts_with("y:") && s.title != "y:0");
}

#[test]
#[ignore]
fn unhandled_keys_come_back() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><p>text</p>\
         <script>addEventListener('keydown',e=>{document.title='k:'+e.key+':'+e.metaKey;\
         if(e.key==='x')e.preventDefault()})</script>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 50);
    fake.forget();
    tap(&fake, 1, 65, 0, 97, 0); // a letter the page sees and does not use
    fake.title_is(1, "k:a:false");
    tap(&fake, 1, 76, 37, 108, modifier::COMMAND); // a browser shortcut
    fake.title_is(1, "k:l:true");
    tap(&fake, 1, 88, 7, 120, 0); // the page prevents this one's default
    fake.title_is(1, "k:x:false");
    tap(&fake, 1, 66, 11, 98, 0); // a sentinel: it comes back, so what is missing is missing
    tap(&fake, 1, 67, 8, 99, 0); // and everything before it has been told by the time this is
    fake.wait(SHORT, |e| match e {
        Event::UnhandledKey {
            kind: KeyKind::Up,
            key,
            ..
        } if key.vk == 67 => Some(()),
        _ => None,
    });
    let seen: Vec<(KeyKind, i32, u32)> = fake
        .history()
        .iter()
        .filter_map(|e| match e {
            Event::UnhandledKey { kind, key, .. } => Some((*kind, key.vk, key.mods)),
            _ => None,
        })
        .collect();
    assert!(seen.contains(&(KeyKind::Down, 65, 0)), "{seen:?}");
    assert!(
        seen.contains(&(KeyKind::Down, 76, modifier::COMMAND)),
        "{seen:?}"
    );
    assert!(
        seen.contains(&(KeyKind::Up, 76, modifier::COMMAND)),
        "{seen:?}"
    );
    // The key the page used does not come back as a key press; its release does (the page did not
    // consume that), so a shortcut is taken from the `Down`.
    assert!(!seen.contains(&(KeyKind::Down, 88, 0)), "{seen:?}");
    assert!(seen.contains(&(KeyKind::Up, 88, 0)), "{seen:?}");
}

#[test]
#[ignore]
fn a_select_list_is_a_popup_of_its_own() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><select style='position:absolute;left:10px;top:10px;width:150px'>\
         <option>one<option>two<option>three</select>",
    );
    fake.open(1, profile("a", None), 400, 300, 2.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 20);
    let (x, y, w, h) = fake.wait(SHORT, |e| match e {
        Event::Popup {
            show: true,
            x,
            y,
            width,
            height,
            ..
        } => Some((*x, *y, *width, *height)),
        _ => None,
    });
    // Device pixels of the view (scale 2), below the control it opened from.
    assert!(w >= 150 && h >= 40 && y >= 20 && x >= 0, "{x} {y} {w} {h}");
    wait_until("popup frames", || fake.popup_frames(1) > 0);
    tap(&fake, 1, KEY_ESCAPE, 53, 27, 0);
    fake.wait(SHORT, |e| {
        matches!(e, Event::Popup { show: false, .. }).then_some(())
    });
}

// ---- requests to bateri ----

#[test]
#[ignore]
fn a_right_click_is_a_menu_request() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <a href='http://other.test/target' style='position:absolute;left:10px;top:10px;font-size:30px'>a link</a>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    right_click(&fake, 1, 30, 30);
    let (req, link, items) = fake.wait(SHORT, |e| match e {
        Event::Menu {
            req, link, items, ..
        } => Some((*req, link.clone(), items.clone())),
        _ => None,
    });
    assert_eq!(link.as_deref(), Some("http://other.test/target"));
    assert!(
        items
            .iter()
            .any(|m| m.enabled && !m.label.is_empty() && !m.separator),
        "{items:?}"
    );
    assert!(
        items.first().is_some_and(|m| !m.separator) && items.last().is_some_and(|m| !m.separator)
    );
    // Dismissed: nothing happens, and the next menu still comes.
    fake.send(Command::MenuResult { req, command: None });
    right_click(&fake, 1, 300, 250);
    let (req, items) = fake.wait(SHORT, |e| match e {
        Event::Menu { req, items, .. } => Some((*req, items.clone())),
        _ => None,
    });
    // Labels come without the engine's mnemonic markers (`&Back`).
    assert!(items.iter().all(|m| !m.label.contains('&')), "{items:?}");
    let back = items
        .iter()
        .find(|m| m.label == "Back" && m.enabled)
        .unwrap_or_else(|| panic!("no Back in {items:?}"));
    fake.send(Command::MenuResult {
        req,
        command: Some(back.command),
    });
    fake.state_where(1, |s| s.url == "about:blank");
}

#[test]
#[ignore]
fn js_dialogs_are_requests_with_answers() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <button style='position:absolute;left:0;top:0;width:100px;height:30px' onclick=\"document.title='c:'+confirm('sure?')\">c</button>\
         <button style='position:absolute;left:0;top:50px;width:100px;height:30px' onclick=\"document.title='p:'+prompt('name?','def')\">p</button>\
         <button style='position:absolute;left:0;top:100px;width:100px;height:30px' onclick=\"alert('hey');document.title='a:done'\">a</button>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    let ask = |fake: &mut Fake| {
        fake.wait(SHORT, |e| match e {
            Event::Dialog {
                req,
                kind,
                message,
                default_text,
                ..
            } => Some((*req, *kind, message.clone(), default_text.clone())),
            _ => None,
        })
    };
    fake.click(1, 50, 15);
    let (req, kind, message, _) = ask(&mut fake);
    assert_eq!((kind, message.as_str()), (DialogKind::Confirm, "sure?"));
    fake.send(Command::DialogResult {
        req,
        ok: true,
        text: String::new(),
    });
    fake.title_is(1, "c:true");
    fake.click(1, 50, 65);
    let (req, kind, message, default_text) = ask(&mut fake);
    assert_eq!(
        (kind, message.as_str(), default_text.as_str()),
        (DialogKind::Prompt, "name?", "def")
    );
    fake.send(Command::DialogResult {
        req,
        ok: true,
        text: "Ada".into(),
    });
    fake.title_is(1, "p:Ada");
    fake.click(1, 50, 115);
    let (req, kind, _, _) = ask(&mut fake);
    assert_eq!(kind, DialogKind::Alert);
    fake.send(Command::DialogResult {
        req,
        ok: true,
        text: String::new(),
    });
    fake.title_is(1, "a:done");
}

#[test]
#[ignore]
fn a_file_input_is_a_file_dialog_request() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <input type=file style='position:absolute;left:0;top:0;width:250px;height:40px' \
         onchange=\"document.title='f:'+this.files.length+':'+this.files[0].name\">",
    );
    let dir = scratch("files");
    let file = dir.join("note.txt");
    std::fs::write(&file, "hi").unwrap();
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 20, 20);
    let (req, mode) = fake.wait(SHORT, |e| match e {
        Event::FileDialog { req, mode, .. } => Some((*req, *mode)),
        _ => None,
    });
    assert_eq!(mode, FileMode::Open);
    // Cancelled first: the page hears nothing. Then chosen.
    fake.send(Command::FileResult { req, paths: vec![] });
    fake.click(1, 20, 20);
    let (req, _) = fake.wait(SHORT, |e| match e {
        Event::FileDialog { req, mode, .. } => Some((*req, *mode)),
        _ => None,
    });
    fake.send(Command::FileResult {
        req,
        paths: vec![file.to_string_lossy().into_owned()],
    });
    fake.title_is(1, "f:1:note.txt");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn a_download_is_a_request_and_nothing_is_written_until_answered() {
    let mut fake = Fake::start();
    fake.proxy.respond(|_| Reply {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "application/octet-stream".into()),
            (
                "Content-Disposition".into(),
                "attachment; filename=\"f.bin\"".into(),
            ),
        ],
        body: b"0123456789".to_vec(),
    });
    let dir = scratch("dl");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://dl.test/f.bin".into(),
    });
    let (req, name, size) = fake.wait(SHORT, |e| match e {
        Event::Download {
            req, name, size, ..
        } => Some((*req, name.clone(), *size)),
        _ => None,
    });
    assert_eq!(name, "f.bin");
    assert_eq!(size, Some(10));
    assert!(std::fs::read_dir(&dir).unwrap().next().is_none());
    let dest = dir.join("saved.bin");
    fake.send(Command::DownloadResult {
        req,
        path: Some(dest.to_string_lossy().into_owned()),
    });
    fake.wait(SHORT, |e| match e {
        Event::DownloadProgress {
            req: r,
            state: DownloadState::Done,
            received,
            ..
        } if *r == req => {
            assert_eq!(*received, 10);
            Some(())
        }
        _ => None,
    });
    assert_eq!(std::fs::read(&dest).unwrap(), b"0123456789");
    // Refused: nothing is written.
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://dl.test/g.bin".into(),
    });
    let (req, _, _) = fake.wait(SHORT, |e| match e {
        Event::Download {
            req, name, size, ..
        } => Some((*req, name.clone(), *size)),
        _ => None,
    });
    fake.send(Command::DownloadResult { req, path: None });
    fake.wait(SHORT, |e| match e {
        Event::DownloadProgress {
            req: r,
            state: DownloadState::Canceled,
            ..
        } if *r == req => Some(()),
        _ => None,
    });
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn a_page_asking_for_a_window_asks_bateri() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <button style='position:absolute;left:0;top:0;width:100px;height:30px' onclick=\"window.open('http://other.test/')\">w</button>\
         <a href='http://bg.test/' target=_blank style='position:absolute;left:0;top:100px;font-size:30px'>link</a>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 15);
    let (url, background) = fake.wait(SHORT, |e| match e {
        Event::Window {
            url, background, ..
        } => Some((url.clone(), *background)),
        _ => None,
    });
    assert_eq!((url.as_str(), background), ("http://other.test/", false));
    // ⌘-click: a tab behind the one in front.
    for kind in [MouseKind::Move, MouseKind::Down, MouseKind::Up] {
        fake.send(Command::Mouse {
            browser: 1,
            kind,
            x: 20,
            y: 115,
            button: Button::Left,
            clicks: 1,
            mods: modifier::COMMAND,
        });
    }
    let (url, background) = fake.wait(SHORT, |e| match e {
        Event::Window {
            url, background, ..
        } => Some((url.clone(), *background)),
        _ => None,
    });
    assert_eq!((url.as_str(), background), ("http://bg.test/", true));
}

// ---- the page's own controls ----

#[test]
#[ignore]
fn find_zoom_reload_and_history() {
    let mut fake = Fake::start();
    fake.proxy.respond(|seen| {
        let body = format!(
            "<title>{p}</title><body style='font-size:20px'>needle one, needle two, needle three",
            p = seen.path
        );
        Reply::html(&body)
    });
    fake.open(1, profile("a", None), 500, 300, 1.0);
    fake.go(1, "http://page.test/a");
    fake.send(Command::Find {
        browser: 1,
        pattern: "needle".into(),
        case: false,
        forward: true,
        next: false,
    });
    // The count and the first match come together: "1 of 3".
    fake.state_where(1, |s| {
        s.found
            == Some(Found {
                count: 3,
                current: 1,
            })
    });
    assert!(
        !fake.history().iter().any(|e| matches!(StateView::of(e, 1), Some(s) if s.found == Some(Found { count: 3, current: 0 }))),
        "a search bar would flash 0 of 3"
    );
    fake.send(Command::Find {
        browser: 1,
        pattern: "needle".into(),
        case: false,
        forward: true,
        next: true,
    });
    fake.state_where(1, |s| {
        s.found
            == Some(Found {
                count: 3,
                current: 2,
            })
    });
    fake.send(Command::StopFinding {
        browser: 1,
        keep_selection: false,
    });
    fake.state_where(1, |s| s.found.is_none());

    fake.send(Command::Zoom {
        browser: 1,
        ratio: 1.44,
    });
    let s = fake.state_where(1, |s| (s.zoom - 1.44).abs() < 0.01);
    assert!((s.zoom - 1.44).abs() < 0.01);

    let before = requests_for(&fake, "/a").len();
    fake.send(Command::Reload {
        browser: 1,
        ignore_cache: true,
    });
    wait_until("the reload reaching the proxy", || {
        requests_for(&fake, "/a").len() > before
    });

    fake.go(1, "http://page.test/b");
    fake.send(Command::Back { browser: 1 });
    let s = fake.state_where(1, |s| s.url.ends_with("/a") && s.can_forward);
    // The blank page the browser started on is still behind it.
    assert!(s.can_back);
    fake.send(Command::Forward { browser: 1 });
    fake.state_where(1, |s| s.url.ends_with("/b") && s.can_back);
}

// ---- the agent's operations ----

#[test]
#[ignore]
fn agent_text_outline_click_type_and_picture() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><h1 style='margin:0;height:60px'>Welcome</h1>\
         <button style='position:absolute;left:10px;top:100px;width:100px;height:40px' \
         onclick=\"document.title='clicked'+(window.n=(window.n||0)+1)\">Go</button>\
         <input aria-label='Name' style='position:absolute;left:10px;top:200px;width:200px;height:30px' \
         oninput=\"document.title='typed:'+this.value\" \
         onkeydown=\"if(event.key==='Enter')document.title='enter:'+this.value\">",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });

    let AgentOutput::Text { text, truncated } = fake.agent(1, AgentOp::Text) else {
        panic!("not text")
    };
    assert!(text.contains("Welcome") && !truncated, "{text:?}");

    let AgentOutput::Text { text: outline, .. } = fake.agent(1, AgentOp::Snapshot) else {
        panic!("not text")
    };
    let node = |role_and_name: &str| -> i64 {
        let line = outline
            .lines()
            .find(|l| l.contains(role_and_name))
            .unwrap_or_else(|| panic!("no {role_and_name} in\n{outline}"));
        let open = line.rfind('[').expect("a node reference");
        line[open + 1..line.len() - 1].parse().expect("a number")
    };
    let button = node("button \"Go\"");
    let textbox = node("textbox \"Name\"");

    assert_eq!(
        fake.agent(
            1,
            AgentOp::Click {
                target: Target::Node { id: button }
            }
        ),
        AgentOutput::Done
    );
    fake.title_is(1, "clicked1");
    assert_eq!(
        fake.agent(
            1,
            AgentOp::Click {
                target: Target::Point { x: 50.0, y: 120.0 }
            }
        ),
        AgentOutput::Done
    );
    fake.title_is(1, "clicked2");
    assert_eq!(
        fake.agent(
            1,
            AgentOp::Type {
                text: "hello".into(),
                target: Some(Target::Node { id: textbox }),
                enter: true
            }
        ),
        AgentOutput::Done
    );
    fake.title_is(1, "enter:hello");
    assert!(matches!(
        fake.agent(
            1,
            AgentOp::Click {
                target: Target::Node { id: 999_999 }
            }
        ),
        AgentOutput::Failed { .. }
    ));

    let AgentOutput::Png { data } = fake.agent(1, AgentOp::Shot) else {
        panic!("not a picture")
    };
    let png = base64::engine::general_purpose::STANDARD
        .decode(data)
        .expect("base64");
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert!(png.len() > 200);
}

// ---- profiles ----

#[test]
#[ignore]
fn two_profiles_keep_their_cookies_apart_and_across_restarts() {
    let root = scratch("profiles");
    let respond = |seen: &Seen| {
        if seen.path == "/set" {
            Reply::html("<title>set</title>").header("Set-Cookie", "sid=1; Max-Age=3600; Path=/")
        } else {
            Reply::html("<title>page</title>")
        }
    };
    {
        let mut fake = Fake::start_with(Options {
            root: Some(root.clone()),
            ..Default::default()
        });
        fake.proxy.respond(respond);
        fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
        fake.open(2, profile("b", Some("pb")), 200, 100, 1.0);
        fake.go(1, "http://site.test/set");
        fake.go(1, "http://site.test/");
        fake.go(2, "http://site.test/");
        let page = requests_for(&fake, "/");
        let of = |user: &str| {
            page.iter()
                .find(|s| s.user == user)
                .unwrap_or_else(|| panic!("no {user}: {page:?}"))
        };
        assert!(
            of("user-a")
                .cookie
                .as_deref()
                .is_some_and(|c| c.contains("sid=1")),
            "{page:?}"
        );
        assert_eq!(of("user-b").cookie, None, "{page:?}");
        assert_eq!(fake.quit(), Some(0));
    }
    // The cookie was written on the way out: a second start on the same data has it.
    let mut fake = Fake::start_with(Options {
        root: Some(root.clone()),
        ..Default::default()
    });
    fake.proxy.respond(respond);
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.open(2, profile("b", Some("pb")), 200, 100, 1.0);
    fake.go(1, "http://site.test/");
    fake.go(2, "http://site.test/");
    let page = requests_for(&fake, "/");
    assert!(
        page.iter().any(|s| s.user == "user-a" && s.cookie.as_deref().is_some_and(|c| c.contains("sid=1"))),
        "{page:?}"
    );
    assert!(
        page.iter()
            .any(|s| s.user == "user-b" && s.cookie.is_none()),
        "{page:?}"
    );
    assert_eq!(fake.quit(), Some(0));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore]
fn a_profile_cannot_be_opened_with_other_settings_or_a_bad_folder() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title>");
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    for (context, dir) in [
        ("a", Some("other")),
        ("b", Some("pa")),
        ("c", Some("../escape")),
        ("d", Some(".hidden")),
    ] {
        fake.send(Command::Open {
            browser: 9,
            profile: profile(context, dir),
            width: 100,
            height: 100,
            scale: 1.0,
        });
        fake.wait(SHORT, |e| {
            matches!(
                e,
                Event::Error {
                    browser: Some(9),
                    ..
                }
            )
            .then_some(())
        });
    }
    // The number of a browser in use is not given twice.
    fake.send(Command::Open {
        browser: 1,
        profile: profile("a", Some("pa")),
        width: 100,
        height: 100,
        scale: 1.0,
    });
    fake.wait(SHORT, |e| {
        matches!(
            e,
            Event::Error {
                browser: Some(1),
                ..
            }
        )
        .then_some(())
    });
    // A command for a browser that is not there is told so.
    fake.send(Command::Back { browser: 77 });
    fake.wait(SHORT, |e| {
        matches!(
            e,
            Event::Error {
                browser: Some(77),
                ..
            }
        )
        .then_some(())
    });
    assert!(fake.go(1, "http://page.test/").error.is_none());
}

// ---- trouble and the end ----

#[test]
#[ignore]
fn a_renderer_that_dies_is_reported_and_the_browser_recovers() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title>");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    // Our own children, killed the way a crash would. Chromium keeps a spare renderer warm, so
    // which one holds the page is not known: all of them go.
    let renderers: Vec<u32> = fake
        .subprocesses()
        .into_iter()
        .filter(|(_, kind)| kind == "renderer")
        .map(|(pid, _)| pid)
        .collect();
    assert!(!renderers.is_empty());
    for pid in renderers {
        unsafe { libc_kill(pid as i32, 9) };
    }
    fake.wait(SHORT, |e| {
        matches!(e, Event::Crashed { browser: 1, .. }).then_some(())
    });
    fake.send(Command::Reload {
        browser: 1,
        ignore_cache: false,
    });
    fake.state_where(1, |s| !s.loading && s.title == "t");
}

unsafe fn libc_kill(pid: i32, signal: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    unsafe { kill(pid, signal) };
}

/// A single-page site (YouTube) going to its next page: a click gives the page a new address
/// (`history.pushState`) and new content. The title says when the event happened and when the page
/// handled it (`event.timeStamp`, `performance.now()`).
const NEXT_PAGE_ON_A_CLICK: &str = "<title>start</title><body style='margin:0'>\
    <button id=b style='position:fixed;left:0;top:0;width:400px;height:300px'>next</button><script>\
    document.addEventListener('keydown', e => {\
      document.title = 'key ' + e.timeStamp + ' ' + performance.now();\
    });\
    b.addEventListener('click', e => {\
      history.pushState({}, '', '/next');\
      const main = document.createElement('main');\
      main.innerHTML = '<h1>The next page</h1><p>' + 'Some of its text. '.repeat(100) + '</p>';\
      document.body.replaceChildren(main);\
      document.title = 'click ' + e.timeStamp + ' ' + performance.now();\
    });\
    </script>";

/// How long before the page handled it an event happened, in milliseconds, from a title of
/// `NEXT_PAGE_ON_A_CLICK`'s; `None` for an event the page sees as happening before it began.
fn event_lag(title: &str) -> Option<f64> {
    let mut parts = title.split(' ').skip(1).map(|n| n.parse::<f64>().unwrap());
    let (stamp, now) = (parts.next().unwrap(), parts.next().unwrap());
    (stamp > 0.0).then_some(now - stamp)
}

/// Chromium dates a "soft navigation" (a click that changes the address and the content) from the
/// click's own time, and checks that this is after the page began. The engine dates the input the
/// helper hands it by the application's current event; once the helper had run the system's event
/// queue for a window of its own (the developer tools'), that event was an old one, the click came
/// before the page, and the check took the page's process down (`TS_PROCESS_CRASHED 5` on
/// YouTube). Shows a window on screen for a moment.
#[test]
#[ignore]
fn input_is_dated_when_it_is_sent_and_a_single_page_sites_click_keeps_the_page_alive() {
    let mut fake = Fake::start();
    serve(&fake, NEXT_PAGE_ON_A_CLICK);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://before.test/");
    let on_screen = |fake: &Fake| fake.windows().iter().filter(|w| w.on_screen).count();
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the developer tools on screen", || on_screen(&fake) == 1);
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the developer tools off screen", || on_screen(&fake) == 0);
    // The last event the window's queue left behind is older than the page that comes next.
    std::thread::sleep(Duration::from_millis(500));
    fake.go(1, "http://page.test/");
    type_char(&fake, 1, 'a');
    let key = fake.state_where(1, |s| s.title.starts_with("key "));
    fake.click(1, 100, 100);
    let click = fake.state_where(1, |s| s.title.starts_with("click "));
    assert_eq!(click.url, "http://page.test/next");
    // The check runs once the new content is painted.
    let crashed = fake.try_wait(SHORT, |e| {
        matches!(e, Event::Crashed { .. }).then(|| format!("{e:?}"))
    });
    assert_eq!(crashed, None);
    let AgentOutput::Text { text, .. } = fake.agent(1, AgentOp::Text) else {
        panic!("not text")
    };
    assert!(text.contains("The next page"), "{text:?}");
    for state in [key, click] {
        let lag = event_lag(&state.title);
        assert!(
            lag.is_some_and(|ms| (0.0..1000.0).contains(&ms)),
            "{}",
            state.title
        );
    }
}

#[test]
#[ignore]
fn closing_a_browser_answers_what_it_had_open() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><button style='position:absolute;left:0;top:0;width:100px;height:30px' \
         onclick=\"document.title='c:'+confirm('sure?')\">c</button>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 15);
    fake.wait(SHORT, |e| matches!(e, Event::Dialog { .. }).then_some(()));
    fake.send(Command::Close { browser: 1 });
    fake.wait(SHORT, |e| {
        matches!(e, Event::Closed { browser: 1 }).then_some(())
    });
    // The number is free again.
    fake.open(1, profile("a", None), 300, 200, 1.0);
    assert!(fake.go(1, "http://page.test/").error.is_none());
}

#[test]
#[ignore]
fn quitting_leaves_no_process_behind_and_a_hang_up_quits() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title>");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    let children: Vec<u32> = fake
        .subprocesses()
        .into_iter()
        .map(|(pid, _)| pid)
        .collect();
    assert!(children.len() >= 2, "{children:?}");
    assert_eq!(fake.quit(), Some(0));
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    wait_until("the helper's processes to be gone", || {
        children
            .iter()
            .all(|&pid| unsafe { kill(pid as i32, 0) } != 0)
    });

    let mut fake = Fake::start();
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.hang_up();
    assert_eq!(fake.exit_code(Duration::from_secs(15)), Some(0));
}

#[test]
#[ignore]
fn nothing_is_granted_and_nobody_is_asked() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><p>text</p><script>\
         (async()=>{let g='none';try{await navigator.mediaDevices.getUserMedia({audio:true});g='granted'}catch(e){g=e.name}\
         const n=await Notification.requestPermission();\
         const l=await new Promise(r=>navigator.geolocation.getCurrentPosition(()=>r('granted'),e=>r('code'+e.code)));\
         document.title='g:'+g+';n:'+n+';l:'+l})()</script>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    // `localhost` is a secure context, where these APIs exist.
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://localhost:8123/".into(),
    });
    let state = fake.state_where(1, |s| s.title.starts_with("g:"));
    assert_eq!(state.title, "g:NotAllowedError;n:denied;l:code1");
    // Nobody was asked: no request of any kind reached bateri.
    assert!(
        !fake
            .history()
            .iter()
            .any(|e| matches!(e, Event::Dialog { .. } | Event::Menu { .. }))
    );
}

#[test]
#[ignore]
fn a_termination_signal_ends_it_in_order() {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    let root = scratch("term");
    let mut fake = Fake::start_with(Options {
        root: Some(root.clone()),
        ..Default::default()
    });
    fake.proxy.respond(|_| {
        Reply::html("<title>t</title><body>text").header("Set-Cookie", "sid=1; Max-Age=3600")
    });
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    // Our own child, asked politely the way `kill` does.
    unsafe { kill(fake.pid as i32, 15) };
    assert_eq!(fake.exit_code(Duration::from_secs(15)), Some(0));
    // (That the cookies are written on the way out is `two_profiles_keep_their_cookies_apart_and_across_restarts`.)
    assert!(root.join("pa").exists());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore]
fn a_page_with_many_origins_is_answered_every_time_with_the_right_identity() {
    // The helper stops answering the proxy after a burst of questions (a proxy that keeps
    // refusing would otherwise loop for ever). A page that asks for dozens of origins at once with
    // the right identity must never meet that limit.
    const HOSTS: usize = 60;
    let page: &'static str = Box::leak(
        format!(
            "<title>many</title><body>{}",
            (1..=HOSTS)
                .map(|n| format!("<img src='http://h{n}.test/p'>"))
                .collect::<String>()
        )
        .into_boxed_str(),
    );
    let mut fake = Fake::start();
    fake.proxy.respond(move |seen| {
        if seen.path == "/p" {
            Reply::status(204)
        } else {
            Reply::html(page)
        }
    });
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    wait_until("every origin reaching the proxy", || {
        requests_for(&fake, "/p").len() >= HOSTS
    });
    assert!(requests_for(&fake, "/p").iter().all(|s| s.user == "user-a"));
    assert!(
        fake.proxy.refused().len() < 16,
        "{} refusals",
        fake.proxy.refused().len()
    );
}

/// A key of the Mac keyboard as the system reports it, with the editing commands its key bindings
/// made of it (`doCommandBySelector:`): a key down that carries them, and the key up.
struct Chord {
    vk: i32,
    native: i32,
    character: u16,
    unmodified: u16,
    mods: u32,
    commands: &'static [&'static str],
}

const CMD_LEFT: Chord = Chord {
    vk: 37,
    native: 123,
    character: 0xF702,
    unmodified: 0xF702,
    mods: modifier::COMMAND,
    commands: &["moveToBeginningOfLine:"],
};
const CMD_RIGHT: Chord = Chord {
    vk: 39,
    native: 124,
    character: 0xF703,
    unmodified: 0xF703,
    mods: modifier::COMMAND,
    commands: &["moveToEndOfLine:"],
};
const CMD_UP: Chord = Chord {
    vk: 38,
    native: 126,
    character: 0xF700,
    unmodified: 0xF700,
    mods: modifier::COMMAND,
    commands: &["moveToBeginningOfDocument:"],
};
const CMD_DOWN: Chord = Chord {
    vk: 40,
    native: 125,
    character: 0xF701,
    unmodified: 0xF701,
    mods: modifier::COMMAND,
    commands: &["moveToEndOfDocument:"],
};
const CMD_SHIFT_LEFT: Chord = Chord {
    vk: 37,
    native: 123,
    character: 0xF702,
    unmodified: 0xF702,
    mods: modifier::COMMAND | modifier::SHIFT,
    commands: &["moveToBeginningOfLineAndModifySelection:"],
};
const CMD_BACKSPACE: Chord = Chord {
    vk: 8,
    native: 51,
    character: 0x7F,
    unmodified: 0x7F,
    mods: modifier::COMMAND,
    commands: &["deleteToBeginningOfLine:"],
};
const CMD_A: Chord = Chord {
    vk: 65,
    native: 0,
    character: 'a' as u16,
    unmodified: 'a' as u16,
    mods: modifier::COMMAND,
    commands: &["selectAll:"],
};
const CTRL_A: Chord = Chord {
    vk: 65,
    native: 0,
    character: 1,
    unmodified: 'a' as u16,
    mods: modifier::CONTROL,
    commands: &["moveToBeginningOfParagraph:"],
};
const CTRL_E: Chord = Chord {
    vk: 69,
    native: 14,
    character: 5,
    unmodified: 'e' as u16,
    mods: modifier::CONTROL,
    commands: &["moveToEndOfParagraph:"],
};
const CTRL_K: Chord = Chord {
    vk: 75,
    native: 40,
    character: 11,
    unmodified: 'k' as u16,
    mods: modifier::CONTROL,
    commands: &["deleteToEndOfParagraph:"],
};

fn chord(fake: &Fake, id: u32, c: &Chord) {
    let data = KeyData {
        vk: c.vk,
        native: c.native,
        character: c.character,
        unmodified: c.unmodified,
        mods: c.mods,
        system: c.mods & modifier::COMMAND != 0,
    };
    fake.send(Command::Key {
        browser: id,
        kind: KeyKind::Down,
        key: data,
        commands: c.commands.iter().map(|n| n.to_string()).collect(),
    });
    fake.send(Command::Key {
        browser: id,
        kind: KeyKind::Up,
        key: data,
        commands: vec![],
    });
}

/// A page with a field and a text area whose value the title reports after every edit (`v:`) and
/// after the Q key goes down (`done<n>:<value>:<last key>`; a key down without its character
/// inserts nothing, so it marks "every key before has been handled").
const EDITING_PAGE: &str = "<title>t</title><body style='margin:0'><p>text</p>\
    <input id=i style='position:absolute;left:10px;top:100px;width:200px;height:30px' value=''>\
    <textarea id=t style='position:absolute;left:10px;top:150px;width:200px;height:80px'></textarea>\
    <script>let n=0;let k='';\
    addEventListener('input',e=>{document.title='v:'+e.target.value});\
    addEventListener('keydown',e=>{if(e.key==='q'){\
      document.title='done'+(++n)+':'+(document.activeElement.value??'')+':'+k}\
      else{k=e.key+':'+e.metaKey}});\
    </script>";

/// Where the field and the text area are, to click into.
const FIELD: (i32, i32) = (150, 115);
const AREA: (i32, i32) = (150, 190);

/// Sends Q (no character) and waits for the page to say it handled it: the page has taken every key before it.
/// `round` counts the marks of this page from 1. Returns the title it made.
fn marked(fake: &mut Fake, round: u32) -> String {
    tap(fake, 1, 81, 12, 'q' as u16, 0);
    let prefix = format!("done{round}:");
    fake.state_where(1, |s| s.title.starts_with(&prefix)).title
}

fn unhandled_downs(fake: &Fake, vk: i32) -> usize {
    fake.history()
        .iter()
        .filter(
            |e| matches!(e, Event::UnhandledKey { kind: KeyKind::Down, key, .. } if key.vk == vk),
        )
        .count()
}

#[test]
#[ignore]
fn command_arrows_come_back_outside_a_field_and_edit_inside_one() {
    let mut fake = Fake::start();
    serve(&fake, EDITING_PAGE);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    // Nothing focused: the page's script sees ⌘← as a key of its own (`ArrowLeft`, command down),
    // nobody has a use for the command, and it comes back, the way the browserpen's back key needs.
    chord(&fake, 1, &CMD_LEFT);
    assert_eq!(marked(&mut fake, 1), "done1::ArrowLeft:true");
    chord(&fake, 1, &CMD_RIGHT);
    marked(&mut fake, 2);
    assert_eq!(
        (unhandled_downs(&fake, 37), unhandled_downs(&fake, 39)),
        (1, 1),
        "{}",
        fake.recent(20)
    );
    // In the field: ⌘← takes the caret to the line's start and does not come back. Typing after it
    // shows where the caret is.
    fake.click(1, FIELD.0, FIELD.1);
    for c in "hello".chars() {
        type_char(&fake, 1, c);
    }
    chord(&fake, 1, &CMD_LEFT);
    type_char(&fake, 1, 'X');
    chord(&fake, 1, &CMD_RIGHT);
    type_char(&fake, 1, 'Y');
    assert!(
        marked(&mut fake, 3).starts_with("done3:XhelloY:"),
        "{}",
        fake.recent(20)
    );
    assert_eq!(
        (unhandled_downs(&fake, 37), unhandled_downs(&fake, 39)),
        (1, 1),
        "the field used both: {}",
        fake.recent(20)
    );
}

/// Starts the editing page, clicks into the field and hands the browser over.
fn editing_page() -> Fake {
    let mut fake = Fake::start();
    serve(&fake, EDITING_PAGE);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake
}

fn type_text(fake: &Fake, text: &str) {
    for c in text.chars() {
        type_char(fake, 1, c);
    }
}

#[test]
#[ignore]
fn a_page_that_prevents_a_key_stops_its_commands() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <input id=i style='position:absolute;left:10px;top:100px;width:200px;height:30px' value=''>\
         <script>let n=0;\
         addEventListener('keydown',e=>{if(e.key==='q'){document.title='done'+(++n)+':'+i.value;return}\
           if(e.metaKey&&e.key==='ArrowLeft')e.preventDefault()});\
         </script>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, FIELD.0, FIELD.1);
    type_text(&fake, "hello");
    // The page takes ⌘← for itself: the caret stays, and the key is not offered to bateri either
    // (the page used it). ⌘→ the page leaves alone, and the field takes it.
    chord(&fake, 1, &CMD_LEFT);
    type_char(&fake, 1, 'X');
    chord(&fake, 1, &CMD_RIGHT);
    type_char(&fake, 1, 'Y');
    assert_eq!(marked(&mut fake, 1), "done1:helloXY");
    assert_eq!(unhandled_downs(&fake, 37), 0, "{}", fake.recent(20));
    assert_eq!(unhandled_downs(&fake, 39), 0, "{}", fake.recent(20));
}

#[test]
#[ignore]
fn the_systems_line_and_document_commands_edit_a_field_and_a_text_area() {
    let mut fake = editing_page();
    let at = |fake: &mut Fake, round: u32| {
        let title = marked(fake, round);
        title.split(':').nth(1).unwrap_or("?").to_string()
    };
    fake.click(1, FIELD.0, FIELD.1);
    type_text(&fake, "one two");
    // ⌃A and ⌃E: start and end of the line.
    chord(&fake, 1, &CTRL_A);
    type_char(&fake, 1, '<');
    chord(&fake, 1, &CTRL_E);
    type_char(&fake, 1, '>');
    assert_eq!(at(&mut fake, 1), "<one two>");
    // ⌃K deletes to the line's end, from the start: all of it.
    chord(&fake, 1, &CTRL_A);
    chord(&fake, 1, &CTRL_K);
    type_text(&fake, "new line");
    assert_eq!(at(&mut fake, 2), "new line");
    // ⌘⌫ deletes back to the line's start, ⌘⇧← selects back to it and the next key replaces.
    chord(&fake, 1, &CMD_BACKSPACE);
    type_text(&fake, "again");
    chord(&fake, 1, &CMD_SHIFT_LEFT);
    type_char(&fake, 1, 'Z');
    assert_eq!(at(&mut fake, 3), "Z");
    // ⌘A selects all and typing replaces it.
    type_text(&fake, "abc");
    chord(&fake, 1, &CMD_A);
    type_char(&fake, 1, 'K');
    assert_eq!(at(&mut fake, 4), "K");
    // None of that was offered to bateri (the letters typed in between come back, they are not
    // the chords: those carry a modifier).
    let offered = fake
        .history()
        .iter()
        .filter(
            |e| matches!(e, Event::UnhandledKey { kind: KeyKind::Down, key, .. } if key.mods != 0),
        )
        .count();
    assert_eq!(offered, 0, "{}", fake.recent(20));
    // The document commands in a text area: ⌘↑ the very start, ⌘↓ the very end, over two lines.
    fake.click(1, AREA.0, AREA.1);
    type_char(&fake, 1, 'a');
    key(&fake, 1, KeyKind::Down, 13, 36, '\r' as u16, 0);
    key(&fake, 1, KeyKind::Char, 13, 36, '\r' as u16, 0);
    key(&fake, 1, KeyKind::Up, 13, 36, '\r' as u16, 0);
    type_char(&fake, 1, 'b');
    chord(&fake, 1, &CMD_UP);
    type_char(&fake, 1, '^');
    chord(&fake, 1, &CMD_DOWN);
    type_char(&fake, 1, '$');
    // marked() reports the focused element's value: the text area's.
    // (the title folds a line break into a space)
    assert_eq!(at(&mut fake, 5), "^a b$");
}

/// The tests that run in the same machine as a person share their system pasteboard: its text is
/// put back when the test is over, however it ends.
struct Pasteboard {
    saved: Vec<u8>,
}

impl Pasteboard {
    fn take() -> Self {
        let out = std::process::Command::new("pbpaste")
            .output()
            .expect("pbpaste");
        Self { saved: out.stdout }
    }

    fn set(&self, text: &str) {
        self.put(text.as_bytes());
    }

    fn put(&self, bytes: &[u8]) {
        use std::io::Write;
        let mut child = std::process::Command::new("pbcopy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("pbcopy");
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        child.wait().unwrap();
    }

    fn text(&self) -> String {
        let out = std::process::Command::new("pbpaste")
            .output()
            .expect("pbpaste");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

impl Drop for Pasteboard {
    fn drop(&mut self) {
        self.put(&self.saved);
    }
}

#[test]
#[ignore]
fn the_edit_menu_copies_cuts_pastes_and_undoes_with_the_system_pasteboard() {
    let board = Pasteboard::take();
    let token = format!("bateri-test-{}", std::process::id());
    board.set("before");
    let mut fake = editing_page();
    let edit = |fake: &Fake, action: EditAction| fake.send(Command::Edit { browser: 1, action });
    fake.click(1, FIELD.0, FIELD.1);
    type_text(&fake, &token);
    let value = |fake: &mut Fake, round: u32| {
        let title = marked(fake, round);
        title.split(':').nth(1).unwrap_or("?").to_string()
    };
    assert_eq!(value(&mut fake, 1), token);
    // Select all and copy: the text is on the system's pasteboard.
    edit(&fake, EditAction::SelectAll);
    edit(&fake, EditAction::Copy);
    wait_until("the page's selection on the pasteboard", || {
        board.text() == token
    });
    // Paste at the end of the field: the text is there twice.
    chord(&fake, 1, &CMD_RIGHT);
    edit(&fake, EditAction::Paste);
    assert_eq!(value(&mut fake, 2), format!("{token}{token}"));
    // Cut takes it out and puts it on the pasteboard; undo and redo walk back and forth.
    board.set("other");
    edit(&fake, EditAction::SelectAll);
    edit(&fake, EditAction::Cut);
    assert_eq!(value(&mut fake, 3), "");
    wait_until("the cut text on the pasteboard", || {
        board.text() == format!("{token}{token}")
    });
    edit(&fake, EditAction::Undo);
    assert_eq!(value(&mut fake, 4), format!("{token}{token}"));
    edit(&fake, EditAction::Redo);
    assert_eq!(value(&mut fake, 5), "");
    // Outside a field the page's own selection is what is copied.
    fake.click(1, 380, 20);
    edit(&fake, EditAction::SelectAll);
    edit(&fake, EditAction::Copy);
    wait_until("the page's text on the pasteboard", || {
        board.text().contains("text")
    });
}

#[test]
#[ignore]
fn keys_and_commands_in_any_mix_keep_their_order() {
    let mut fake = editing_page();
    fake.click(1, FIELD.0, FIELD.1);
    // A fixed pseudo-random run of letters and commands, sent back to back with nothing between:
    // the text must come out as if every key had been taken in the order it was sent.
    let mut text = String::new();
    let mut caret = 0usize;
    let mut seed = 0x2545F491u32;
    for round in 1..=3u32 {
        for _ in 0..120 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            match (seed >> 24) % 8 {
                0 => {
                    chord(&fake, 1, &CMD_LEFT);
                    caret = 0;
                }
                1 => {
                    chord(&fake, 1, &CMD_RIGHT);
                    caret = text.len();
                }
                2 => {
                    chord(&fake, 1, &CMD_BACKSPACE);
                    text.replace_range(..caret, "");
                    caret = 0;
                }
                3 => {
                    chord(&fake, 1, &CTRL_K);
                    text.truncate(caret);
                }
                _ => {
                    let c = (b'a' + ((seed >> 16) % 16) as u8) as char; // never `q`: the page's mark
                    type_char(&fake, 1, c);
                    text.insert(caret, c);
                    caret += 1;
                }
            }
        }
        // Start each round from where the caret is known to be.
        chord(&fake, 1, &CMD_RIGHT);
        caret = text.len();
        let title = marked(&mut fake, round);
        assert_eq!(
            title.split(':').nth(1).unwrap_or("?"),
            text,
            "round {round}: {}",
            fake.recent(6)
        );
    }
}

#[test]
#[ignore]
fn the_developer_tools_command_opens_a_window_and_the_next_one_closes_it() {
    // The helper is a background app: the window the engine makes for the developer tools has to
    // be ordered in and the helper's event queue run, or it exists and is never seen. This one
    // shows a window on screen for a moment.
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title><body>a page</body>");
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    let on_screen = |fake: &Fake| fake.windows().iter().filter(|w| w.on_screen).count();
    assert_eq!(on_screen(&fake), 0, "{:?}", fake.windows());
    fake.send(Command::DevTools { browser: 1 });
    wait_until("a window of the helper's own on screen", || {
        on_screen(&fake) == 1
    });
    // The same command again takes it away (the closed window may stay in the list, off screen).
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the window off screen", || on_screen(&fake) == 0);
}

/// What bateri's proxy answers when it cannot reach the server: a 502 with a sentence for the
/// person and the Chromium error it stands for in a header.
fn proxy_failure(code: i32, sentence: &str) -> Reply {
    Reply {
        status: 502,
        headers: vec![
            ("Content-Type".into(), "text/plain; charset=utf-8".into()),
            ("X-Bateri-Net-Error".into(), code.to_string()),
        ],
        body: sentence.as_bytes().to_vec(),
    }
}

#[test]
#[ignore]
fn a_pages_own_document_the_proxy_could_not_fetch_is_a_load_error() {
    let mut fake = Fake::start();
    fake.proxy.respond(|seen| match seen.path.as_str() {
        "/refused" => proxy_failure(-102, "localhost:8799: Connection refused (os error 61)"),
        "/timeout" => proxy_failure(-118, "the server did not answer in time"),
        "/plain-502" => Reply {
            status: 502,
            headers: vec![("Content-Type".into(), "text/plain".into())],
            body: b"a bad gateway of some other proxy".to_vec(),
        },
        // A site's own answer carrying the header is not the proxy's.
        "/spoof" => Reply {
            status: 200,
            headers: vec![
                ("Content-Type".into(), "text/html".into()),
                ("X-Bateri-Net-Error".into(), "-102".into()),
            ],
            body: b"<title>spoof</title>a page".to_vec(),
        },
        "/image" => Reply::html(
            "<title>with image</title><img src='/missing.png'>\
             <iframe src='/missing-frame'></iframe>",
        ),
        "/missing.png" | "/missing-frame" => proxy_failure(-102, "nothing listens there"),
        _ => Reply::html("<title>fine</title>fine"),
    });
    fake.open(1, profile("a", None), 400, 300, 1.0);
    // The document itself failed: the error names the page, the code and the error's name, and
    // the load is over.
    fake.go(1, "http://page.test/refused");
    let state = fake.state_where(1, |s| s.error.is_some());
    assert!(!state.loading, "{state:?}");
    let error = state.error.expect("the load failed");
    assert_eq!(error.url, "http://page.test/refused");
    assert_eq!(error.code, -102);
    assert_eq!(error.message, "ERR_CONNECTION_REFUSED");
    // The next page starts clean, and a failure of another kind has its own code.
    let state = fake.go(1, "http://page.test/");
    assert!(state.error.is_none(), "{state:?}");
    fake.go(1, "http://page.test/timeout");
    let state = fake.state_where(1, |s| s.error.is_some());
    assert_eq!(state.error.map(|e| e.code), Some(-118));
    // A 502 with a body and without the header is a page like any other (with no body at all
    // the engine makes its own error of it, as it always did).
    let state = fake.go(1, "http://page.test/plain-502");
    assert!(state.error.is_none(), "{state:?}");
    let state = fake.go(1, "http://page.test/spoof");
    assert!(state.error.is_none(), "{state:?}");
    fake.title_is(1, "spoof");
    // What a page loads for itself (a picture, a frame) fails without an error of the page's.
    let state = fake.go(1, "http://page.test/image");
    assert!(state.error.is_none(), "{state:?}");
    fake.title_is(1, "with image");
    std::thread::sleep(Duration::from_millis(500));
    let last = fake
        .history()
        .iter()
        .rev()
        .find_map(|e| StateView::of(e, 1))
        .expect("a state");
    assert!(last.error.is_none(), "{last:?}");
}

#[test]
#[ignore]
fn the_helper_does_not_register_with_the_notification_center() {
    // Chromium's macOS notification bridge starts a helper of its own ("(Alerts)") that registers
    // the app with the system's notification center, and the system then asks the person to allow
    // notifications for "bateri Browser". The helper never shows notifications (a page's request
    // is refused), so it must never register. The system's notification daemon logs every
    // connection with the path of the app it comes from: none may come from the app under test.
    let started = {
        let out = std::process::Command::new("date")
            .arg("+%Y-%m-%d %H:%M:%S")
            .output()
            .expect("date");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title><body>a page</body>");
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    // The registration comes within a second of the start; the log takes a moment to be readable.
    std::thread::sleep(Duration::from_secs(3));
    let app = app_executable();
    let bundle = app
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
        .unwrap();
    let predicate = format!(
        "process == \"usernoted\" AND eventMessage CONTAINS \"{}\"",
        bundle.display()
    );
    let out = std::process::Command::new("/usr/bin/log")
        .args([
            "show",
            "--info",
            "--debug",
            "--start",
            &started,
            "--predicate",
            &predicate,
        ])
        .output()
        .expect("log show");
    let found: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.contains("usernoted"))
        .map(str::to_string)
        .collect();
    assert!(
        found.is_empty(),
        "the helper registered with the notification center:\n{}",
        found.join("\n")
    );
}

// ---- the network guarantee: nothing leaves except through the proxy ----

/// Whether `haystack` holds `needle`.
fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    // SAFETY: both are valid slices for their lengths.
    unsafe {
        !libc::memmem(
            haystack.as_ptr().cast(),
            haystack.len(),
            needle.as_ptr().cast(),
            needle.len(),
        )
        .is_null()
    }
}

/// The engine's binary inside the bundle under test.
fn engine_binary() -> Vec<u8> {
    let app = app_executable();
    let bundle = app
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent());
    let path = bundle.expect("bundle").join(
        "Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework",
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
#[ignore]
fn every_switch_and_preference_the_helper_names_is_one_the_engine_has() {
    // The engine ignores a switch it does not know without a word: three of the first ones
    // (`dns-prefetch-disable`, `no-proxy-server-autodetect-fallback`,
    // `force-webrtc-ip-handling-policy`) were never in force. A switch name is a string the
    // engine carries between two NULs (a substring would find `disable-sync` in other switches).
    let engine = engine_binary();
    let exact = |name: &str| contains_bytes(&engine, format!("\0{name}\0").as_bytes());
    let missing: Vec<&str> = switches::names()
        .into_iter()
        .filter(|n| !exact(n))
        .collect();
    assert!(missing.is_empty(), "the engine has no switch {missing:?}");
    let missing: Vec<&str> = switches::PREFS
        .iter()
        .map(|(name, _)| *name)
        .filter(|n| !exact(n))
        .collect();
    assert!(
        missing.is_empty(),
        "the engine has no preference {missing:?}"
    );
    // A feature's name is looked up as text only (the engine keeps some inside longer strings).
    let missing: Vec<&str> = switches::DISABLED_FEATURES
        .iter()
        .copied()
        .filter(|f| !contains_bytes(&engine, f.as_bytes()))
        .collect();
    assert!(missing.is_empty(), "the engine has no feature {missing:?}");
    // And the policy the engine takes by name.
    assert!(
        exact(switches::WEBRTC_POLICY),
        "the engine does not know {}",
        switches::WEBRTC_POLICY
    );
}

#[test]
#[ignore]
fn webrtc_does_not_send_udp_outside_the_proxy() {
    // A page asks WebRTC to gather candidates with a STUN server on loopback. Left alone the
    // engine sends a UDP packet there (past the HTTP proxy, which cannot carry UDP); held to
    // `disable_non_proxied_udp` it sends nothing at all.
    let sink = std::net::UdpSocket::bind("127.0.0.1:0").expect("a UDP sink");
    sink.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let port = sink.local_addr().unwrap().port();
    let mut fake = Fake::start();
    let page = format!(
        "<title>t</title><body><script>\
         const pc = new RTCPeerConnection({{iceServers: [{{urls: 'stun:127.0.0.1:{port}'}}]}});\
         pc.createDataChannel('x');\
         pc.createOffer().then(o => pc.setLocalDescription(o));\
         document.title = 'ice:started';\
         setTimeout(() => document.title = 'ice:' + pc.iceGatheringState, 4000);</script>"
    );
    fake.proxy.respond(move |_| Reply::html(&page));
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://page.test/".into(),
    });
    // The control: the page ran, asked WebRTC to gather, and four seconds went by (a gathering
    // that has not finished by then is still waiting for a STUN server that never answers; no
    // packet means nothing unless the page got as far as asking).
    let state = fake.state_where(1, |s| {
        s.title.starts_with("ice:") && s.title != "ice:started"
    });
    assert!(
        state.title == "ice:complete" || state.title == "ice:gathering",
        "{state:?}"
    );
    let mut buf = [0u8; 1500];
    let mut packets = 0;
    while sink.recv_from(&mut buf).is_ok() {
        packets += 1;
    }
    assert_eq!(
        packets, 0,
        "WebRTC sent UDP to a STUN server outside the proxy"
    );
}

#[test]
#[ignore]
fn the_network_process_is_told_to_resolve_no_name_but_the_proxys() {
    // Page host names go to the proxy as names; nothing in the process resolves one itself. The
    // rule has to reach the engine's network process, where lookups happen.
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title><body>a page</body>");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://page.test/");
    let want = format!("--host-resolver-rules={}", switches::HOST_RESOLVER_RULES);
    let children = fake.children();
    let network: Vec<&(u32, String)> = children
        .iter()
        .filter(|(_, c)| c.contains("network.mojom.NetworkService"))
        .collect();
    assert!(!network.is_empty(), "no network process in {children:?}");
    for (pid, command) in network {
        assert!(
            command.contains(&want),
            "process {pid} has no resolver rule: {command}"
        );
    }
}

// ---- the developer tools window ----

#[test]
#[ignore]
fn a_closed_developer_tools_window_is_not_brought_back_by_the_next_one() {
    // The closed window stays in the application's list; asking for the developer tools of
    // another browser must bring forward only the window that request made.
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title><body>a page</body>");
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.open(2, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.go(2, "http://page.test/");
    let on_screen = |fake: &Fake| fake.windows().iter().filter(|w| w.on_screen).count();
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the first window on screen", || on_screen(&fake) == 1);
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the first window off screen", || on_screen(&fake) == 0);
    fake.send(Command::DevTools { browser: 2 });
    wait_until("the second window on screen", || on_screen(&fake) >= 1);
    // The first one would be brought back a moment after, by the same turn of the loop.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(on_screen(&fake), 1, "{:?}", fake.windows());
    fake.send(Command::DevTools { browser: 2 });
    wait_until("the second window off screen", || on_screen(&fake) == 0);
    // Asked for again after it was closed, the same browser gets a window that is brought forward.
    fake.send(Command::DevTools { browser: 2 });
    wait_until("a new window on screen", || on_screen(&fake) >= 1);
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(on_screen(&fake), 1, "{:?}", fake.windows());
    fake.send(Command::DevTools { browser: 2 });
    wait_until("the last window off screen", || on_screen(&fake) == 0);
}

#[test]
#[ignore]
fn the_developer_tools_page_loads_in_its_window_under_the_helpers_own_client() {
    // The tools' window has a client of its own (the page's would stand for the page itself),
    // which holds the tools' navigation to `devtools://` and sends a window they ask for to
    // bateri. The helper traces what the client is told; that is the one place its being there
    // shows (the initial load of the tools' page is not a navigation the client is asked about).
    // SAFETY: the test binary runs one test at a time (`--test-threads=1`), and the variable is
    // only read by the helper process started right here.
    unsafe { std::env::set_var("BATERI_BROWSER_TRACE", "1") };
    let mut fake = Fake::start();
    unsafe { std::env::remove_var("BATERI_BROWSER_TRACE") };
    serve(&fake, "<title>t</title><body>a page</body>");
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    let renderers = |fake: &Fake| {
        fake.subprocesses()
            .iter()
            .filter(|(_, kind)| kind == "renderer")
            .count()
    };
    let before = renderers(&fake);
    fake.send(Command::DevTools { browser: 1 });
    // The tools' page runs in a renderer of its own, and the client was told it was made.
    wait_until("the tools' page to have a renderer of its own", || {
        renderers(&fake) > before
    });
    wait_until("the tools' own client to hear of its window", || {
        fake.log()
            .contains("the developer tools of browser 1 were created")
    });
    // Nothing was refused on the way, and nobody was asked for a window.
    let stray = fake.try_wait(Duration::from_secs(2), |e| match e {
        Event::Refused { .. } | Event::Window { .. } => Some(format!("{e:?}")),
        _ => None,
    });
    assert_eq!(stray, None);
    fake.send(Command::DevTools { browser: 1 });
}

// ---- the context menu ----

/// `cef_menu_id_t::MENU_ID_VIEW_SOURCE`: the engine's "view page source" command.
const MENU_ID_VIEW_SOURCE: i32 = 132;

#[test]
#[ignore]
fn only_a_command_the_menu_offered_is_run_and_view_source_is_not_offered() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <a href='http://other.test/target' style='position:absolute;left:10px;top:10px;font-size:30px'>a link</a>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    right_click(&fake, 1, 300, 250);
    let (req, items) = fake.wait(SHORT, |e| match e {
        Event::Menu { req, items, .. } => Some((*req, items.clone())),
        _ => None,
    });
    assert!(!items.is_empty());
    assert!(
        items
            .iter()
            .all(|m| m.command != MENU_ID_VIEW_SOURCE && !m.label.to_lowercase().contains("source")),
        "{items:?}"
    );
    // An answer that names a command the menu did not hold (view-source opens an address the
    // helper never loads): nothing happens, and the next menu still comes.
    fake.forget();
    fake.send(Command::MenuResult {
        req,
        command: Some(MENU_ID_VIEW_SOURCE),
    });
    right_click(&fake, 1, 300, 250);
    let req2 = fake.wait(SHORT, |e| match e {
        Event::Menu { req, .. } => Some(*req),
        _ => None,
    });
    assert_ne!(req, req2);
    let stray: Vec<&Event> = fake
        .history()
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::Window { .. } | Event::Refused { .. } | Event::Error { .. }
            )
        })
        .collect();
    assert!(stray.is_empty(), "{stray:?}");
    // One it did hold still runs (Back is there once the page has history).
    fake.send(Command::MenuResult {
        req: req2,
        command: None,
    });
}

// ---- downloads ----

fn download_reply(body: &[u8]) -> Reply {
    Reply {
        status: 200,
        headers: vec![
            ("Content-Type".into(), "application/octet-stream".into()),
            (
                "Content-Disposition".into(),
                "attachment; filename=\"f.bin\"".into(),
            ),
        ],
        body: body.to_vec(),
    }
}

fn progress_of(e: &Event, want: u32) -> Option<DownloadState> {
    match e {
        Event::DownloadProgress { req, state, .. } if *req == want => Some(*state),
        _ => None,
    }
}

#[test]
#[ignore]
fn a_download_that_breaks_off_is_over_and_says_so() {
    let mut fake = Fake::start();
    fake.proxy
        .respond(|_| download_reply(&[7u8; 100]).cut_after(10));
    let dir = scratch("dl-cut");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://dl.test/f.bin".into(),
    });
    let req = fake.wait(SHORT, |e| match e {
        Event::Download { req, .. } => Some(*req),
        _ => None,
    });
    let dest = dir.join("cut.bin");
    fake.send(Command::DownloadResult {
        req,
        path: Some(dest.to_string_lossy().into_owned()),
    });
    // An interrupted download does not come back: the last word is `Canceled`, once.
    fake.wait(Duration::from_secs(10), |e| {
        (progress_of(e, req) == Some(DownloadState::Canceled)).then_some(())
    });
    std::thread::sleep(Duration::from_millis(500));
    let ends = fake
        .history()
        .iter()
        .filter(|e| progress_of(e, req) == Some(DownloadState::Canceled))
        .count();
    assert_eq!(ends, 1, "{}", fake.recent(8));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn closing_the_page_cancels_the_download_it_started() {
    let mut fake = Fake::start();
    fake.proxy
        .respond(|_| download_reply(&[7u8; 100]).stalled_after(10));
    let dir = scratch("dl-close");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://dl.test/f.bin".into(),
    });
    let req = fake.wait(SHORT, |e| match e {
        Event::Download { req, .. } => Some(*req),
        _ => None,
    });
    fake.send(Command::DownloadResult {
        req,
        path: Some(dir.join("slow.bin").to_string_lossy().into_owned()),
    });
    fake.wait(SHORT, |e| {
        (progress_of(e, req) == Some(DownloadState::Running)).then_some(())
    });
    fake.send(Command::Close { browser: 1 });
    fake.wait(Duration::from_secs(10), |e| {
        (progress_of(e, req) == Some(DownloadState::Canceled)).then_some(())
    });
    std::thread::sleep(Duration::from_millis(500));
    let ends = fake
        .history()
        .iter()
        .filter(|e| progress_of(e, req) == Some(DownloadState::Canceled))
        .count();
    assert_eq!(ends, 1, "{}", fake.recent(8));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
#[ignore]
fn a_refused_download_is_canceled_once() {
    let mut fake = Fake::start();
    fake.proxy.respond(|_| download_reply(b"0123456789"));
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.send(Command::Navigate {
        browser: 1,
        url: "http://dl.test/f.bin".into(),
    });
    let req = fake.wait(SHORT, |e| match e {
        Event::Download { req, .. } => Some(*req),
        _ => None,
    });
    fake.send(Command::DownloadResult { req, path: None });
    fake.wait(SHORT, |e| {
        (progress_of(e, req) == Some(DownloadState::Canceled)).then_some(())
    });
    std::thread::sleep(Duration::from_secs(1));
    let ends = fake
        .history()
        .iter()
        .filter(|e| progress_of(e, req).is_some_and(|s| s != DownloadState::Running))
        .count();
    assert_eq!(ends, 1, "the same request ended twice: {}", fake.recent(8));
}

#[test]
#[ignore]
fn a_pages_own_blob_is_an_ordinary_download() {
    // The way a web app saves what it made: not an address of anyone else's, so not refused.
    // (In an inline handler `URL` is `document.URL`, a string: the page says `window.URL`.)
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <button style='position:absolute;left:0;top:0;width:100px;height:30px' \
         onclick=\"const a=document.createElement('a');a.href=window.URL.createObjectURL(new Blob(['hello blob']));a.download='b.txt';a.click()\">d</button>",
    );
    let dir = scratch("dl-blob");
    fake.open(1, profile("a", None), 200, 100, 1.0);
    fake.go(1, "http://dl.test/blob");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 15);
    let (req, name) = fake.wait(SHORT, |e| match e {
        Event::Download { req, name, .. } => Some((*req, name.clone())),
        _ => None,
    });
    assert_eq!(name, "b.txt");
    let dest = dir.join("b.txt");
    fake.send(Command::DownloadResult {
        req,
        path: Some(dest.to_string_lossy().into_owned()),
    });
    fake.wait(SHORT, |e| {
        (progress_of(e, req) == Some(DownloadState::Done)).then_some(())
    });
    assert_eq!(std::fs::read(&dest).unwrap(), b"hello blob");
    assert!(
        !fake
            .history()
            .iter()
            .any(|e| matches!(e, Event::Refused { .. }))
    );
    let _ = std::fs::remove_dir_all(dir);
}

// ---- questions that go void, and work that goes unanswered ----

#[test]
#[ignore]
fn a_page_that_moves_on_voids_the_question_it_had_open() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><button style='position:absolute;left:0;top:0;width:100px;height:30px' \
         onclick=\"document.title='c:'+confirm('sure?')\">c</button>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 15);
    let req = fake.wait(SHORT, |e| match e {
        Event::Dialog { req, .. } => Some(*req),
        _ => None,
    });
    fake.send(Command::Navigate {
        browser: 1,
        url: "about:blank".into(),
    });
    // bateri is told the question is void, once.
    let void = fake.wait(SHORT, |e| match e {
        Event::DialogVoid { browser: 1, req } => Some(*req),
        _ => None,
    });
    assert_eq!(void, req);
    // And an answer that arrives late is ignored without a word.
    fake.send(Command::DialogResult {
        req,
        ok: true,
        text: String::new(),
    });
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        fake.history()
            .iter()
            .filter(|e| matches!(e, Event::DialogVoid { .. }))
            .count(),
        1
    );
    assert!(
        !fake
            .history()
            .iter()
            .any(|e| matches!(e, Event::Error { .. })),
        "{}",
        fake.recent(6)
    );
}

#[test]
#[ignore]
fn closing_with_a_leave_page_question_open_closes() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><button style='position:absolute;left:0;top:0;width:100px;height:30px'>b</button>\
         <script>window.onbeforeunload = e => { e.preventDefault(); return 'x'; }</script>",
    );
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    // The question only comes for a page the person has used.
    fake.click(1, 50, 15);
    fake.send(Command::Navigate {
        browser: 1,
        url: "about:blank".into(),
    });
    let (req, kind) = fake.wait(SHORT, |e| match e {
        Event::Dialog { req, kind, .. } => Some((*req, *kind)),
        _ => None,
    });
    assert_eq!(kind, DialogKind::BeforeUnload);
    let _ = req;
    fake.send(Command::Close { browser: 1 });
    fake.wait(SHORT, |e| {
        matches!(e, Event::Closed { browser: 1 }).then_some(())
    });
}

#[test]
#[ignore]
fn an_agent_job_for_a_browser_that_never_came_up_is_answered() {
    let mut fake = Fake::start();
    let p = profile("a", None);
    fake.proxy.accept(&p.user, &p.pass);
    // Open, ask, close: all before the engine has made the browser.
    fake.send(Command::Open {
        browser: 1,
        profile: p,
        width: 200,
        height: 100,
        scale: 1.0,
    });
    fake.send(Command::Agent {
        browser: 1,
        req: 11,
        op: AgentOp::Text,
    });
    fake.send(Command::Close { browser: 1 });
    let output = fake.wait(DEFAULT_WAIT_LONG, |e| match e {
        Event::AgentResult {
            req: 11, output, ..
        } => Some(output.clone()),
        _ => None,
    });
    assert!(matches!(output, AgentOutput::Failed { .. }), "{output:?}");
    // And for a browser nobody opened.
    let output = fake.agent(9, AgentOp::Text);
    assert!(matches!(output, AgentOutput::Failed { .. }), "{output:?}");
}

const DEFAULT_WAIT_LONG: Duration = Duration::from_secs(20);

#[test]
#[ignore]
fn agent_jobs_sent_before_the_browser_is_up_are_each_answered_once() {
    let mut fake = Fake::start();
    let p = profile("a", None);
    fake.proxy.accept(&p.user, &p.pass);
    fake.send(Command::Open {
        browser: 1,
        profile: p,
        width: 200,
        height: 100,
        scale: 1.0,
    });
    for req in 1000..1300 {
        fake.send(Command::Agent {
            browser: 1,
            req,
            op: AgentOp::Text,
        });
    }
    // Every job gets exactly one answer, whether it ran once the browser was up or was turned
    // away at the door. How many were turned away depends on how fast the browser comes up against
    // how fast 300 commands arrive, so the count is not asserted here: that the queue hands a
    // command back at its limit is a unit test in `helper/src/commands.rs`.
    let mut answered = std::collections::HashMap::new();
    while answered.len() < 300 {
        let (req, output) = fake.wait(DEFAULT_WAIT_LONG, |e| match e {
            Event::AgentResult { req, output, .. } if (1000..1300).contains(req) => {
                Some((*req, output.clone()))
            }
            _ => None,
        });
        assert!(
            answered.insert(req, output).is_none(),
            "{req} answered twice"
        );
    }
    let again = fake.try_wait(Duration::from_millis(500), |e| match e {
        Event::AgentResult { req, .. } if (1000..1300).contains(req) => Some(*req),
        _ => None,
    });
    assert_eq!(again, None, "a job was answered twice");
}

#[test]
#[ignore]
fn quitting_with_a_question_and_a_download_open_leaves_in_order() {
    let mut fake = Fake::start();
    fake.proxy.respond(|seen| match seen.path.as_str() {
        "/f.bin" => download_reply(&[7u8; 100]).stalled_after(10),
        _ => Reply::html(
            "<title>t</title><body style='margin:0'><button style='position:absolute;left:0;top:0;width:100px;height:30px' \
             onclick=\"document.title='c:'+confirm('sure?')\">c</button>",
        ),
    });
    let dir = scratch("quit-open");
    fake.open(1, profile("a", None), 300, 200, 1.0);
    fake.open(2, profile("a", None), 300, 200, 1.0);
    fake.send(Command::Navigate {
        browser: 2,
        url: "http://dl.test/f.bin".into(),
    });
    let req = fake.wait(SHORT, |e| match e {
        Event::Download { req, .. } => Some(*req),
        _ => None,
    });
    fake.send(Command::DownloadResult {
        req,
        path: Some(dir.join("slow.bin").to_string_lossy().into_owned()),
    });
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake.click(1, 50, 15);
    fake.wait(SHORT, |e| matches!(e, Event::Dialog { .. }).then_some(()));
    assert_eq!(fake.quit(), Some(0));
    let _ = std::fs::remove_dir_all(dir);
}

// ---- pinch, and the rows the helper adds to the page's menu ----

/// A page that says what a trackpad pinch did to it through its title: how many `wheel` events it
/// had, whether the last had `ctrlKey`, its direction, the visual viewport's scale, the layout
/// viewport's width, where the last click was and where the last `wheel` event was. At `/?take` it calls `preventDefault` on them
/// (a map or a design canvas does).
const PINCH_PAGE: &str = "<title>t</title><body style='margin:0;width:3000px;height:3000px'>\
    <script>let n=0,ctrl='-',dir='-',click='-',at='-';\
    addEventListener('wheel',e=>{n++;ctrl=e.ctrlKey;dir=e.deltaY<0?'in':'out';\
    at=Math.round(e.clientX)+','+Math.round(e.clientY);\
    if(location.search=='?take')e.preventDefault()},{passive:false});\
    addEventListener('click',e=>{click=Math.round(e.clientX)+','+Math.round(e.clientY)});\
    setInterval(()=>{document.title='n='+n+' ctrl='+ctrl+' dir='+dir+\
    ' s='+visualViewport.scale.toFixed(2)+' iw='+innerWidth+' click='+click+' at='+at},50)</script>";

/// One field of `PINCH_PAGE`'s title.
fn field<'a>(title: &'a str, key: &str) -> Option<&'a str> {
    title
        .split(' ')
        .find_map(|part| part.strip_prefix(key)?.strip_prefix('='))
}

fn pinch(fake: &Fake, kind: PinchKind, x: i32, y: i32, scale: f64) {
    fake.send(Command::Pinch {
        browser: 1,
        kind,
        x,
        y,
        scale,
    });
}

/// The fingers spread by `total` in `steps` equal parts around (200, 150), as a trackpad reports.
fn spread(fake: &Fake, total: f64, steps: u32) {
    pinch(fake, PinchKind::Begin, 200, 150, 1.0);
    for _ in 0..steps {
        pinch(
            fake,
            PinchKind::Update,
            200,
            150,
            total.powf(1.0 / f64::from(steps)),
        );
    }
    pinch(fake, PinchKind::End, 200, 150, 1.0);
}

fn pinch_page(fake: &mut Fake, query: &str) {
    serve(fake, PINCH_PAGE);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, &format!("http://page.test/{query}"));
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
}

#[test]
#[ignore]
fn a_pinch_a_page_takes_is_ctrl_wheel_events_and_nothing_else() {
    let mut fake = Fake::start();
    pinch_page(&mut fake, "?take");
    spread(&fake, 2.0, 4);
    // The page had it as a `wheel` event with `ctrlKey`, the way Chrome on a Mac gives it, and
    // spreading the fingers is a negative `deltaY`.
    fake.state_where(1, |s| {
        field(&s.title, "ctrl") == Some("true") && field(&s.title, "dir") == Some("in")
    });
    // It called `preventDefault`: the view stays as it was, however long it is given.
    std::thread::sleep(Duration::from_millis(800));
    fake.forget();
    let s = fake.state_where(1, |_| true);
    assert_eq!(field(&s.title, "s"), Some("1.00"), "{}", s.title);
    // The next click is where it was aimed.
    fake.click(1, 350, 260);
    let s = fake.state_where(1, |s| field(&s.title, "click") != Some("-"));
    assert_eq!(field(&s.title, "click"), Some("350,260"), "{}", s.title);
}

#[test]
#[ignore]
fn a_pinch_a_page_does_not_take_zooms_the_view_and_clicks_land_where_aimed() {
    let mut fake = Fake::start();
    pinch_page(&mut fake, "");
    // The page is told first (it does not take it), and then the view is zoomed around the
    // fingers: the layout viewport is the width it was (it is not the page zoom of `Zoom`).
    spread(&fake, 2.0, 4);
    let s = fake.state_where(1, |s| field(&s.title, "s") == Some("2.00"));
    assert_eq!(field(&s.title, "ctrl"), Some("true"), "{}", s.title);
    assert_eq!(field(&s.title, "dir"), Some("in"), "{}", s.title);
    assert_eq!(field(&s.title, "iw"), Some("400"), "{}", s.title);
    assert_eq!(fake.state_where(1, |_| true).zoom, 1.0);
    // The page point that was under the fingers, (200, 150), still is: the view's corner is at
    // (100, 75) in the page, so the view's (350, 260) is the page's (275, 205).
    fake.click(1, 350, 260);
    let s = fake.state_where(1, |s| field(&s.title, "click") != Some("-"));
    assert_eq!(field(&s.title, "click"), Some("275,205"), "{}", s.title);
    // A second pinch goes on from there, and a pinch in goes back to the whole page.
    spread(&fake, 0.25, 3);
    fake.state_where(1, |s| field(&s.title, "s") == Some("1.00"));
    fake.click(1, 350, 260);
    fake.state_where(1, |s| field(&s.title, "click") == Some("350,260"));
}

/// Right-clicks the page's empty corner, and answers the menu with the row called `label`.
/// Returns the menu's rows.
fn pick(fake: &mut Fake, label: &str) -> Vec<MenuItem> {
    fake.forget();
    right_click(fake, 1, 300, 250);
    let (req, items) = fake.wait(SHORT, |e| match e {
        Event::Menu { req, items, .. } => Some((*req, items.clone())),
        _ => None,
    });
    let row = items
        .iter()
        .find(|m| m.label == label)
        .unwrap_or_else(|| panic!("no {label} in {items:?}"));
    fake.send(Command::MenuResult {
        req,
        command: Some(row.command),
    });
    items
}

#[test]
#[ignore]
fn the_pages_menu_ends_in_zoom_rows_that_step_by_chromes_presets() {
    let mut fake = Fake::start();
    serve(&fake, "<title>t</title><body style='margin:0'>a page");
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    let items = pick(&mut fake, "Zoom In");
    // At the end of the menu, after the page's own rows: the zoom rows, a line, Inspect.
    let tail: Vec<(&str, &str, bool)> = items[items.len() - 6..]
        .iter()
        .map(|m| {
            let label = if m.separator { "--" } else { m.label.as_str() };
            (label, m.shortcut.as_str(), m.enabled)
        })
        .collect();
    assert_eq!(
        tail,
        vec![
            ("--", "", false),
            ("Zoom In", "⌘+", true),
            ("Zoom Out", "⌘\u{2212}", true),
            // Already at 100 %: nowhere to go.
            ("Actual Size", "⌘0", false),
            ("--", "", false),
            ("Inspect", "⌥⌘I", true),
        ],
        "{items:?}"
    );
    // Chrome's presets (110, 125 %), and what the state event says is what the engine has.
    fake.state_where(1, |s| (s.zoom - 1.1).abs() < 0.005);
    let items = pick(&mut fake, "Zoom In");
    assert!(
        items.iter().any(|m| m.label == "Actual Size" && m.enabled),
        "{items:?}"
    );
    fake.state_where(1, |s| (s.zoom - 1.25).abs() < 0.005);
    pick(&mut fake, "Zoom Out");
    fake.state_where(1, |s| (s.zoom - 1.1).abs() < 0.005);
    pick(&mut fake, "Actual Size");
    fake.state_where(1, |s| (s.zoom - 1.0).abs() < 0.005);
    pick(&mut fake, "Zoom Out");
    fake.state_where(1, |s| (s.zoom - 0.9).abs() < 0.005);
    pick(&mut fake, "Actual Size");
    fake.state_where(1, |s| (s.zoom - 1.0).abs() < 0.005);
    // The row is disabled at 100 %, and an answer that names it anyway is not run.
    fake.forget();
    pick(&mut fake, "Actual Size");
    let moved = fake.try_wait(Duration::from_secs(1), |e| {
        StateView::of(e, 1).filter(|s| (s.zoom - 1.0).abs() > 0.005)
    });
    assert!(moved.is_none(), "{moved:?}");
}

#[test]
#[ignore]
fn the_inspect_row_opens_the_developer_tools_at_the_point_and_a_second_one_stays_in_the_window() {
    // SAFETY: the test binary runs one test at a time (`--test-threads=1`), and the variable is
    // only read by the helper process started right here.
    unsafe { std::env::set_var("BATERI_BROWSER_TRACE", "1") };
    let mut fake = Fake::start();
    unsafe { std::env::remove_var("BATERI_BROWSER_TRACE") };
    serve(
        &fake,
        "<title>t</title><body style='margin:0'>\
         <h1 style='position:absolute;left:10px;top:10px;margin:0'>a heading</h1>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    let on_screen = |fake: &Fake| fake.windows().iter().filter(|w| w.on_screen).count();
    let inspect = |fake: &mut Fake, x: i32, y: i32| {
        fake.forget();
        right_click(fake, 1, x, y);
        let (req, items) = fake.wait(SHORT, |e| match e {
            Event::Menu { req, items, .. } => Some((*req, items.clone())),
            _ => None,
        });
        let row = items
            .last()
            .filter(|m| m.label == "Inspect" && m.enabled)
            .unwrap_or_else(|| panic!("Inspect is not the last row of {items:?}"));
        fake.send(Command::MenuResult {
            req,
            command: Some(row.command),
        });
    };
    assert_eq!(on_screen(&fake), 0, "{:?}", fake.windows());
    inspect(&mut fake, 30, 25);
    wait_until("the tools' window on screen", || on_screen(&fake) == 1);
    // The point the menu was asked at (the heading) is the one the engine was told to look at.
    wait_until("the helper to ask for the tools at the point", || {
        fake.log()
            .contains("developer tools of browser 1 are asked for at 30,25")
    });
    // Where they are open already the engine looks at the element in the same window.
    let windows = fake.windows().len();
    inspect(&mut fake, 300, 250);
    wait_until("the second request to reach the engine", || {
        fake.log()
            .contains("developer tools of browser 1 are asked for at 300,250")
    });
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(on_screen(&fake), 1, "{:?}", fake.windows());
    assert_eq!(fake.windows().len(), windows, "{:?}", fake.windows());
    fake.send(Command::DevTools { browser: 1 });
    wait_until("the window off screen", || on_screen(&fake) == 0);
}

#[test]
#[ignore]
fn a_pinch_on_a_page_zoomed_as_a_page_is_at_the_fingers() {
    let mut fake = Fake::start();
    pinch_page(&mut fake, "?take");
    // At 200 % a point of the view is half as far from the corner in the page's own pixels: the
    // page hears the pinch where the fingers are, not twice as far.
    fake.send(Command::Zoom {
        browser: 1,
        ratio: 2.0,
    });
    fake.state_where(1, |s| (s.zoom - 2.0).abs() < 0.01);
    pinch(&fake, PinchKind::Begin, 200, 100, 1.0);
    pinch(&fake, PinchKind::Update, 200, 100, 1.1);
    pinch(&fake, PinchKind::End, 200, 100, 1.0);
    let s = fake.state_where(1, |s| field(&s.title, "at").is_some_and(|at| at != "-"));
    assert_eq!(field(&s.title, "at"), Some("100,50"), "{}", s.title);
}

#[test]
#[ignore]
fn a_pinch_zoom_on_a_page_zoomed_as_a_page_keeps_the_point_under_the_fingers() {
    let mut fake = Fake::start();
    pinch_page(&mut fake, "");
    fake.send(Command::Zoom {
        browser: 1,
        ratio: 2.0,
    });
    fake.state_where(1, |s| (s.zoom - 2.0).abs() < 0.01);
    spread(&fake, 2.0, 2);
    fake.state_where(1, |s| field(&s.title, "s") == Some("2.00"));
    // The view is 200 x 150 of the page's own pixels now, and the point of it that was under
    // (200, 150) is (100, 75) of them: the view's (350, 260) is the page's (50 + 87.5, 37.5 + 65).
    fake.click(1, 350, 260);
    let s = fake.state_where(1, |s| field(&s.title, "click") != Some("-"));
    let click = field(&s.title, "click").unwrap_or("");
    let (x, y) = click.split_once(',').unwrap_or(("", ""));
    let (x, y): (f64, f64) = (x.parse().unwrap_or(-1.0), y.parse().unwrap_or(-1.0));
    assert!(
        (x - 137.5).abs() <= 1.0 && (y - 102.5).abs() <= 1.0,
        "{}",
        s.title
    );
}

// ---- the rows of a link and of an image in the page's menu ----

/// A one pixel PNG as an address: an image the helper may open (`data:`).
const PIXEL: &str = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg==";

/// A page with a link, an image, an image that is a link, and two links the helper does not load.
fn links_page(fake: &mut Fake) {
    let html = format!(
        "<title>t</title><body style='margin:0'>\
         <a href='http://other.test/target' style='position:absolute;left:10px;top:10px;font-size:30px'>a link</a>\
         <img style='position:absolute;left:10px;top:100px;width:50px;height:50px' src='{PIXEL}'>\
         <a href='http://other.test/img' style='position:absolute;left:100px;top:100px'><img style='width:50px;height:50px' src='{PIXEL}'></a>\
         <a href='mailto:x@y.test' style='position:absolute;left:10px;top:200px;font-size:30px'>mail</a>\
         <a href='javascript:void(0)' style='position:absolute;left:200px;top:200px;font-size:30px'>script</a>"
    );
    fake.proxy.respond(move |_| Reply::html(&html));
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
}

/// Right-clicks (`x`, `y`) and returns the menu that comes: its request number and its rows, as
/// their labels (`--` for a line).
fn menu_over(fake: &mut Fake, x: i32, y: i32) -> (u32, Vec<MenuItem>) {
    fake.forget();
    right_click(fake, 1, x, y);
    fake.wait(SHORT, |e| match e {
        Event::Menu { req, items, .. } => Some((*req, items.clone())),
        _ => None,
    })
}

fn labels(items: &[MenuItem]) -> Vec<&str> {
    items
        .iter()
        .map(|m| if m.separator { "--" } else { m.label.as_str() })
        .collect()
}

fn pick_row(fake: &Fake, req: u32, items: &[MenuItem], label: &str) {
    let row = items
        .iter()
        .find(|m| m.label == label && m.enabled)
        .unwrap_or_else(|| panic!("no {label} in {:?}", labels(items)));
    fake.send(Command::MenuResult {
        req,
        command: Some(row.command),
    });
}

#[test]
#[ignore]
fn a_links_menu_starts_with_the_rows_that_open_it_in_the_background_and_copy_its_address() {
    let mut fake = Fake::start();
    links_page(&mut fake);
    let board = Pasteboard::take();
    let (req, items) = menu_over(&mut fake, 30, 30);
    assert_eq!(
        labels(&items)[..3],
        ["Open Link in New Tab", "Copy Link Address", "--"],
        "{:?}",
        labels(&items)
    );
    // The engine's own rows follow, and the helper's zoom and Inspect are at the end as before.
    assert!(
        items[3..].iter().any(|m| m.label == "Copy"),
        "{:?}",
        labels(&items)
    );
    assert_eq!(labels(&items).last(), Some(&"Inspect"));
    // Opening it is a request to bateri for a window, in the background, and nothing is loaded here.
    pick_row(&fake, req, &items, "Open Link in New Tab");
    let (url, background) = fake.wait(SHORT, |e| match e {
        Event::Window {
            url, background, ..
        } => Some((url.clone(), *background)),
        _ => None,
    });
    assert_eq!(
        (url.as_str(), background),
        ("http://other.test/target", true)
    );
    // Where bateri opens a link in the browser itself the row says so, and asks the same way.
    fake.send(Command::Windows {
        browser: 1,
        mode: NewWindows::Here,
    });
    let (req, items) = menu_over(&mut fake, 30, 30);
    assert_eq!(
        labels(&items)[..2],
        ["Open Link", "Copy Link Address"],
        "{:?}",
        labels(&items)
    );
    pick_row(&fake, req, &items, "Open Link");
    let url = fake.wait(SHORT, |e| match e {
        Event::Window {
            url,
            background: true,
            ..
        } => Some(url.clone()),
        _ => None,
    });
    assert_eq!(url, "http://other.test/target");
    // Copying writes the address as the page wrote it to the system pasteboard.
    board.set("before");
    let (req, items) = menu_over(&mut fake, 30, 30);
    pick_row(&fake, req, &items, "Copy Link Address");
    wait_until("the address on the pasteboard", || {
        board.text() == "http://other.test/target"
    });
}

#[test]
#[ignore]
fn a_link_the_helper_does_not_load_is_copied_and_not_opened() {
    let mut fake = Fake::start();
    links_page(&mut fake);
    let board = Pasteboard::take();
    for (x, y, address) in [
        (30, 220, "mailto:x@y.test"),
        (230, 220, "javascript:void(0)"),
    ] {
        let (req, items) = menu_over(&mut fake, x, y);
        let rows = labels(&items);
        assert!(
            !rows.iter().any(|l| l.starts_with("Open Link")),
            "{address}: {rows:?}"
        );
        assert_eq!(rows[0], "Copy Link Address", "{address}: {rows:?}");
        board.set("before");
        fake.forget();
        pick_row(&fake, req, &items, "Copy Link Address");
        wait_until("the address on the pasteboard", || board.text() == address);
    }
    // Nobody was asked for a window, or refused one.
    let stray = fake.try_wait(Duration::from_secs(1), |e| match e {
        Event::Window { .. } | Event::Refused { .. } => Some(format!("{e:?}")),
        _ => None,
    });
    assert_eq!(stray, None);
}

#[test]
#[ignore]
fn an_images_menu_starts_with_the_rows_that_open_it_in_the_background_and_copy_its_address() {
    let mut fake = Fake::start();
    links_page(&mut fake);
    let board = Pasteboard::take();
    let (req, items) = menu_over(&mut fake, 30, 120);
    assert_eq!(
        labels(&items)[..3],
        ["Open Image in New Tab", "Copy Image Address", "--"],
        "{:?}",
        labels(&items)
    );
    pick_row(&fake, req, &items, "Open Image in New Tab");
    let (url, background) = fake.wait(SHORT, |e| match e {
        Event::Window {
            url, background, ..
        } => Some((url.clone(), *background)),
        _ => None,
    });
    assert_eq!((url.as_str(), background), (PIXEL, true));
    board.set("before");
    let (req, items) = menu_over(&mut fake, 30, 120);
    pick_row(&fake, req, &items, "Copy Image Address");
    wait_until("the address on the pasteboard", || board.text() == PIXEL);
    // An image that is a link has the link's rows first, and then its own.
    let (req, items) = menu_over(&mut fake, 120, 120);
    assert_eq!(
        labels(&items)[..6],
        [
            "Open Link in New Tab",
            "Copy Link Address",
            "--",
            "Open Image in New Tab",
            "Copy Image Address",
            "--"
        ],
        "{:?}",
        labels(&items)
    );
    // Dismissed, it answers nothing.
    fake.send(Command::MenuResult { req, command: None });
}

// ---- the mouse's side buttons and the sideways swipe ----

/// Reports the side buttons it hears through its title, and takes the mouse event the address
/// names (`?mouseup` calls `preventDefault` on the `mouseup`).
const SIDE_PAGE: &str = "<title>t</title><body style='margin:0'><div style='height:300px'>x</div>\
    <script>const hold=location.search.slice(1);const seen=[];\
    for(const t of ['mousedown','mouseup','auxclick'])addEventListener(t,e=>{\
    seen.push(t+e.button);document.title=seen.join(' ');if(t===hold)e.preventDefault()})</script>";

/// The fourth or fifth button of a mouse pressed and let go over the page.
fn side(fake: &Fake, button: Button) {
    fake.mouse(1, MouseKind::Move, 50, 50, Button::Left);
    fake.mouse(1, MouseKind::Down, 50, 50, button);
    fake.mouse(1, MouseKind::Up, 50, 50, button);
}

#[test]
#[ignore]
fn the_side_buttons_go_back_and_forward() {
    let mut fake = Fake::start();
    serve(&fake, SIDE_PAGE);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/a");
    fake.go(1, "http://page.test/b");
    fake.go(1, "http://page.test/c");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    side(&fake, Button::Back);
    fake.state_where(1, |s| s.url == "http://page.test/b" && !s.loading);
    side(&fake, Button::Back);
    let s = fake.state_where(1, |s| s.url == "http://page.test/a" && !s.loading);
    assert!(s.can_forward);
    side(&fake, Button::Forward);
    fake.state_where(1, |s| s.url == "http://page.test/b" && !s.loading);
    side(&fake, Button::Forward);
    let s = fake.state_where(1, |s| s.url == "http://page.test/c" && !s.loading);
    assert!(!s.can_forward);
}

#[test]
#[ignore]
fn a_page_that_takes_the_mouseup_keeps_the_side_button_from_going() {
    let mut fake = Fake::start();
    serve(&fake, SIDE_PAGE);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    // Back: the page hears the button as number 3 (down, up and the aux click), and its
    // `preventDefault` on the `mouseup` is what Chrome honours.
    fake.go(1, "http://page.test/a");
    fake.go(1, "http://page.test/b?mouseup");
    side(&fake, Button::Back);
    fake.title_is(1, "mousedown3 mouseup3 auxclick3");
    std::thread::sleep(Duration::from_millis(700));
    fake.forget();
    let s = fake.state_where(1, |_| true);
    assert_eq!(s.url, "http://page.test/b?mouseup");
    assert!(!s.loading);
    // Forward: the button is number 4, and the same page keeps it.
    fake.go(1, "http://page.test/c?mouseup");
    fake.go(1, "http://page.test/d");
    fake.send(Command::Back { browser: 1 });
    let s = fake.state_where(1, |s| s.url == "http://page.test/c?mouseup" && !s.loading);
    assert!(s.can_forward);
    side(&fake, Button::Forward);
    fake.title_is(1, "mousedown4 mouseup4 auxclick4");
    std::thread::sleep(Duration::from_millis(700));
    fake.forget();
    let s = fake.state_where(1, |_| true);
    assert_eq!(s.url, "http://page.test/c?mouseup");
    // A page that only listens does not stop it.
    fake.go(1, "http://page.test/e?auxclick");
    side(&fake, Button::Back);
    fake.state_where(1, |s| s.url == "http://page.test/c?mouseup" && !s.loading);
}

/// What a sideways scroll at a point meets, one page for each case: the wide page, a box that
/// scrolls sideways (`/box?N` starts it at offset N), the same in a right-to-left page, a page that
/// hides its sideways overflow or keeps its overscroll, a frame of another origin.
fn sideways_pages(fake: &Fake) {
    const BOX: &str = "position:absolute;left:0;top:0;width:200px;height:100px;overflow-x:auto;\
        overflow-y:hidden;white-space:nowrap";
    fake.proxy.respond(|seen| {
        let path = seen.path.as_str();
        let page = |html: &str| Reply::html(&format!("<!doctype html><title>t</title>{html}"));
        if seen.host.starts_with("other.test") {
            return page("<body style='margin:0'>inner");
        }
        if path.starts_with("/wide") {
            page(
                "<body style='margin:0;width:3000px;height:600px'>wide<script>\
                 if(location.search=='?right')scrollTo(99999,0);\
                 addEventListener('scroll',()=>document.title='x:'+Math.round(scrollX))</script>",
            )
        } else if path.starts_with("/box") {
            page(&format!(
                "<body style='margin:0;height:600px'><div id=b style='{BOX}'><div style='width:1000px;\
                 height:50px'>wide</div></div><script>b.scrollLeft=+location.search.slice(1)||0</script>"
            ))
        } else if path.starts_with("/rtl") {
            page(&format!(
                "<body style='margin:0;height:600px'><div id=b dir=rtl style='{BOX}'><div style='width:1000px;\
                 height:50px'>wide</div></div>"
            ))
        } else if path.starts_with("/hidden") {
            Reply::html(
                "<!doctype html><title>t</title><html style='overflow-x:hidden'>\
                 <body style='margin:0;width:3000px;height:600px'>wide",
            )
        } else if path.starts_with("/kept") {
            page("<body style='margin:0;width:3000px;height:600px;overscroll-behavior-x:none'>wide")
        } else if path.starts_with("/quirks") {
            Reply::html("<title>t</title><body style='margin:0;width:3000px;height:600px'>wide")
        } else if path.starts_with("/frame") {
            page(
                "<body style='margin:0;height:600px'><iframe src='http://other.test/inner' \
                 style='position:absolute;left:0;top:0;width:200px;height:100px;border:0'></iframe>",
            )
        } else {
            page("<body style='margin:0;height:600px'>narrow")
        }
    });
}

/// Asks the helper whether a sideways scroll at `(x, y)` would move something (`dx` as a wheel's),
/// and waits for the answer.
fn can_scroll(fake: &mut Fake, x: i32, y: i32, dx: i32) -> bool {
    static REQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(100);
    let req = REQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    fake.send(Command::CanScroll {
        browser: 1,
        req,
        x,
        y,
        dx,
    });
    fake.wait(SHORT, |e| match e {
        Event::CanScroll { req: r, can, .. } if *r == req => Some(*can),
        _ => None,
    })
}

const ABOUT_BACK: i32 = 100;
const ABOUT_FORWARD: i32 = -100;

fn sideways_open(url: &str) -> Fake {
    let mut fake = Fake::start();
    sideways_pages(&fake);
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, &format!("http://page.test/{url}"));
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    fake
}

#[test]
#[ignore]
fn a_wheel_moves_the_page_by_the_sign_of_dx() {
    let mut fake = sideways_open("wide");
    fake.mouse(1, MouseKind::Move, 50, 50, Button::Left);
    // A negative `dx` is the content moving left: the page scrolls to the right.
    fake.send(Command::Wheel {
        browser: 1,
        x: 50,
        y: 50,
        dx: ABOUT_FORWARD * 2,
        dy: 0,
        mods: 0,
    });
    fake.state_where(1, |s| s.title.starts_with("x:") && s.title != "x:0");
    fake.send(Command::Wheel {
        browser: 1,
        x: 50,
        y: 50,
        dx: ABOUT_BACK * 8,
        dy: 0,
        mods: 0,
    });
    fake.title_is(1, "x:0");
}

#[test]
#[ignore]
fn a_page_at_the_edge_lets_the_swipe_go_and_a_page_that_can_scroll_keeps_it() {
    let mut fake = sideways_open("wide");
    // At the left edge of a wide page: the content cannot move right, so that way is history's;
    // the other way the page scrolls.
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // At the right edge it is the other way round.
    fake.go(1, "http://page.test/wide?right");
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // A page that does not scroll sideways has nothing to keep.
    fake.go(1, "http://page.test/narrow");
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // Quirks mode scrolls through the body.
    fake.go(1, "http://page.test/quirks");
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // A page that hides its sideways overflow cannot be scrolled that way, and one that keeps its
    // overscroll does not let a gesture through to history (Chrome's own rule for both).
    fake.go(1, "http://page.test/hidden");
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    fake.go(1, "http://page.test/kept");
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
}

#[test]
#[ignore]
fn a_box_under_the_finger_that_can_scroll_keeps_the_swipe() {
    let mut fake = sideways_open("box?0");
    // The box is at its start: its content cannot move right, and the page has no sideways
    // scroll, so that is history's; the other way the box scrolls. Outside the box, nothing does.
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    assert!(!can_scroll(&mut fake, 300, 200, ABOUT_FORWARD));
    // In the middle of its way it takes both.
    fake.go(1, "http://page.test/box?300");
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    assert!(!can_scroll(&mut fake, 300, 200, ABOUT_BACK));
    // At its far end only the way back is the box's.
    fake.go(1, "http://page.test/box?800");
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // A box that reads right to left starts at its right end.
    fake.go(1, "http://page.test/rtl");
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    // Page zoom moves the point: the view's (350, 50) is the page's (175, 25) at twice the size.
    fake.go(1, "http://page.test/box?300");
    assert!(!can_scroll(&mut fake, 350, 50, ABOUT_BACK));
    fake.send(Command::Zoom {
        browser: 1,
        ratio: 2.0,
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(can_scroll(&mut fake, 350, 50, ABOUT_BACK));
}

#[test]
#[ignore]
fn a_frame_of_another_origin_keeps_the_swipe_and_a_pinched_view_pans_first() {
    let mut fake = sideways_open("frame");
    // What is in the frame cannot be seen from here: the page keeps the gesture, never history.
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
    assert!(!can_scroll(&mut fake, 300, 200, ABOUT_BACK));
    // Zoomed with the fingers the visual viewport has room to move before history is asked: here
    // it is at the left edge of a page that does not scroll sideways.
    fake.go(1, "http://page.test/narrow");
    pinch(&fake, PinchKind::Begin, 0, 150, 1.0);
    for _ in 0..4 {
        pinch(&fake, PinchKind::Update, 0, 150, 2.0_f64.powf(0.25));
    }
    pinch(&fake, PinchKind::End, 0, 150, 1.0);
    std::thread::sleep(Duration::from_millis(800));
    assert!(!can_scroll(&mut fake, 50, 50, ABOUT_BACK));
    assert!(can_scroll(&mut fake, 50, 50, ABOUT_FORWARD));
}

// ---- a profile's data as records ----

const SITE: &str = "http://site.test/";

/// The pages of the profile tests: one sets cookies, one writes Local Storage, one shows what a
/// page finds in its Local Storage as its title.
fn profile_site(seen: &Seen) -> Reply {
    match seen.path.as_str() {
        "/set" => Reply::html("<title>set</title>")
            .header(
                "Set-Cookie",
                "sid=1; Max-Age=3600; Path=/; HttpOnly; SameSite=Lax",
            )
            .header("Set-Cookie", "tmp=2; Path=/"),
        "/write" => Reply::html(
            "<title>w</title><script>localStorage.setItem('token','abc');document.title='written'</script>",
        ),
        "/read" => Reply::html(
            "<title>r</title><script>document.title='token='+localStorage.getItem('token')</script>",
        ),
        "/idb-write" => Reply::html(
            "<title>i</title><script>var r=indexedDB.open('d',1);\
r.onupgradeneeded=function(){r.result.createObjectStore('s')};\
r.onsuccess=function(){var t=r.result.transaction('s','readwrite');t.objectStore('s').put('hello','k');\
t.oncomplete=function(){document.title='idb-written'}}</script>",
        ),
        "/idb-read" => Reply::html(
            "<title>i</title><script>var r=indexedDB.open('d',1);\
r.onupgradeneeded=function(){r.result.createObjectStore('s')};\
r.onsuccess=function(){var g=r.result.transaction('s').objectStore('s').get('k');\
g.onsuccess=function(){document.title='idb='+g.result}}</script>",
        ),
        _ => Reply::html("<title>page</title>"),
    }
}

fn cookie_named<'a>(records: &'a [Record], name: &str) -> Option<&'a CookieValue> {
    records.iter().find_map(|r| match r {
        Record::Cookie {
            name: n,
            value: Some(v),
            ..
        } if n == name => Some(v),
        _ => None,
    })
}

fn item_of<'a>(records: &'a [Record], key: &str) -> Option<&'a str> {
    records.iter().find_map(|r| match r {
        Record::LocalStorage {
            key: k,
            value: Some(v),
            ..
        } if k == key => Some(v.as_str()),
        _ => None,
    })
}

#[test]
#[ignore]
fn a_profile_read_in_one_is_written_in_another_and_its_first_page_is_signed_in() {
    let mut fake = Fake::start();
    fake.proxy.respond(profile_site);
    // Two profiles in one helper.
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.open(2, profile("b", Some("pb")), 200, 100, 1.0);
    fake.go(1, "http://site.test/set");
    fake.go(1, "http://site.test/write");
    fake.title_is(1, "written");

    let records = fake.profile_read(1);
    let sid = cookie_named(&records, "sid").unwrap_or_else(|| panic!("no sid: {records:?}"));
    assert_eq!(sid.value, "1");
    assert!(sid.http_only, "{sid:?}");
    assert_eq!(sid.same_site, 2, "{sid:?}");
    assert!(sid.expires.is_some(), "{sid:?}");
    // A cookie of the session has no end.
    assert_eq!(cookie_named(&records, "tmp").map(|c| c.expires), Some(None));
    assert_eq!(item_of(&records, "token"), Some("abc"), "{records:?}");
    // What the other profile holds is its own: nothing of the first one's.
    let other = fake.profile_read(2);
    assert!(cookie_named(&other, "sid").is_none(), "{other:?}");
    assert!(item_of(&other, "token").is_none(), "{other:?}");

    // Into the second, before it has gone anywhere: the cookie is written at once, the item
    // waits for the first page of its origin.
    let deferred = fake.profile_write(2, records.clone());
    assert_eq!(deferred, 1);
    fake.forget();
    fake.go(2, "http://site.test/read");
    // The first request this profile ever sent carried the cookie (and its httpOnly cookie).
    let first = fake
        .proxy
        .seen()
        .into_iter()
        .find(|s| s.user == "user-b" && s.host.contains("site.test"))
        .expect("a request of the second profile");
    assert!(
        first.cookie.as_deref().is_some_and(|c| c.contains("sid=1")),
        "{first:?}"
    );
    // The page's own script found the item: the seed ran before it.
    fake.title_is(2, "token=abc");
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn a_named_profile_keeps_its_session_cookies_when_the_helper_ends_and_starts_again() {
    let root = scratch("session-cookies");
    {
        let mut fake = Fake::start_with(Options {
            root: Some(root.clone()),
            ..Default::default()
        });
        fake.proxy.respond(profile_site);
        fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
        fake.go(1, "http://site.test/set");
        fake.go(1, "http://site.test/");
        let records = fake.profile_read(1);
        // `tmp` has no end (a cookie of the session) and `sid` has one.
        assert_eq!(
            cookie_named(&records, "tmp").map(|c| c.expires),
            Some(None),
            "{records:?}"
        );
        assert!(cookie_named(&records, "sid").is_some(), "{records:?}");
        assert_eq!(fake.quit(), Some(0));
    }
    // What a profile keeps for its person is what the home keeps for it: a computer that
    // restarts (or whose helper ended after its linger) must not lose the cookies of the session
    // that were roamed to it, or its home and its profile say different things.
    let mut fake = Fake::start_with(Options {
        root: Some(root.clone()),
        ..Default::default()
    });
    fake.proxy.respond(profile_site);
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    let records = fake.profile_read(1);
    assert!(cookie_named(&records, "sid").is_some(), "{records:?}");
    assert_eq!(
        cookie_named(&records, "tmp").map(|c| c.value.as_str()),
        Some("2"),
        "the session cookie is gone after a restart: {records:?}"
    );
    assert_eq!(fake.quit(), Some(0));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore]
fn a_session_cookie_is_kept_also_when_its_profile_is_the_one_the_engine_last_used() {
    let root = scratch("session-cookies-last-used");
    // The engine writes down the profile a window was last used in (bateri's Pi profile is named
    // there on the machine this was found on), and each start loads that profile first, as the
    // engine's own, before any request context exists. The context made for its folder afterwards
    // is the engine's own context, with the engine's settings and not the ones `create_context`
    // gives, so the settings of the engine must keep the cookies of the session too, or that
    // profile's preferences say "a new tab" at the end of every run and the next start finds no
    // cookie of the session. The fake does not put a window in front, so the note is written here.
    std::fs::write(
        root.join("Local State"),
        "{\"profile\":{\"last_used\":\"pa\"}}",
    )
    .unwrap();
    let run = |go: bool| -> Option<String> {
        let mut fake = Fake::start_with(Options {
            root: Some(root.clone()),
            ..Default::default()
        });
        fake.proxy.respond(profile_site);
        fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
        if go {
            fake.go(1, "http://site.test/set");
        }
        let records = fake.profile_read(1);
        assert_eq!(fake.quit(), Some(0));
        cookie_named(&records, "tmp").map(|c| c.value.clone())
    };
    assert_eq!(run(true).as_deref(), Some("2"));
    // Each start after that must find the cookie.
    for n in 2..=4 {
        assert_eq!(
            run(false).as_deref(),
            Some("2"),
            "the session cookie is gone at the start number {n}"
        );
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
#[ignore]
fn changes_come_as_records_and_what_was_written_does_not_come_back() {
    let mut fake = Fake::start();
    fake.proxy.respond(profile_site);
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.go(1, "http://site.test/");
    let base = fake.profile_read(1);
    assert!(cookie_named(&base, "sid").is_none(), "{base:?}");
    fake.profile_watch(1, true);

    // A response that sets cookies is seen at once, not at the next look.
    let mark = fake.mark();
    fake.go(1, "http://site.test/set");
    let (changes, _) = fake
        .changed_since(
            mark,
            Duration::from_secs(10),
            |r| matches!(r, Record::Cookie { name, .. } if name == "sid"),
        )
        .unwrap_or_else(|| panic!("the cookie was not reported: {}", fake.recent(8)));
    assert!(cookie_named(&changes, "tmp").is_some(), "{changes:?}");

    // A page writing Local Storage is reported by its events.
    let mark = fake.mark();
    fake.go(1, "http://site.test/write");
    fake.changed_since(mark, Duration::from_secs(10), |r| {
        matches!(r, Record::LocalStorage { key, value: Some(v), .. } if key == "token" && v == "abc")
    })
    .unwrap_or_else(|| panic!("the item was not reported: {}", fake.recent(8)));

    // What the other end wrote is not a change: neither the cookie nor the item (the page of its
    // origin is open) comes back.
    let cookie = Record::Cookie {
        domain: "site.test".into(),
        path: "/".into(),
        name: "remote".into(),
        partition: None,
        value: Some(CookieValue {
            value: "r1".into(),
            expires: None,
            secure: false,
            http_only: true,
            same_site: 0,
            priority: 1,
            source_scheme: 1,
            source_port: 80,
        }),
    };
    let item = Record::LocalStorage {
        origin: SITE.into(),
        key: "remote-item".into(),
        value: Some("x".into()),
    };
    let mark = fake.mark();
    let deferred = fake.profile_write(1, vec![cookie, item]);
    assert_eq!(deferred, 0);
    let echoed = fake.changed_since(mark, Duration::from_secs(3), |r| match r {
        Record::Cookie { name, .. } => name == "remote",
        Record::LocalStorage { key, .. } => key == "remote-item",
        _ => false,
    });
    assert!(echoed.is_none(), "{echoed:?}");
    // The write did land: a fresh read has both.
    let now = fake.profile_read(1);
    assert!(cookie_named(&now, "remote").is_some(), "{now:?}");
    assert_eq!(item_of(&now, "remote-item"), Some("x"), "{now:?}");

    // A deletion is a record too.
    fake.profile_write(
        1,
        vec![Record::Cookie {
            domain: "site.test".into(),
            path: "/".into(),
            name: "remote".into(),
            partition: None,
            value: None,
        }],
    );
    assert!(cookie_named(&fake.profile_read(1), "remote").is_none());
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn an_indexed_db_goes_by_folder_into_a_profile_that_is_not_running() {
    let mut fake = Fake::start();
    fake.proxy.respond(profile_site);
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.go(1, "http://site.test/idb-write");
    fake.title_is(1, "idb-written");
    std::thread::sleep(Duration::from_secs(1));

    let (records, stage) = fake.indexed_db_read("pa");
    let Some(Record::IndexedDb {
        origin,
        value: Some(value),
    }) = records.first()
    else {
        panic!("no folder: {records:?}");
    };
    assert_eq!(origin, "http_site.test_0");
    assert_eq!(value.format, "chromium-idb-leveldb");
    assert!(
        value.files.iter().any(|f| f.path.starts_with("leveldb/")),
        "{value:?}"
    );
    assert!(
        fake.root
            .join(&stage)
            .join(origin)
            .join(&value.files[0].path)
            .exists()
    );

    // A profile whose engine runs does not take it; one that has not been opened does.
    let (written, deferred) = fake.indexed_db_write("pa", &stage, records.clone());
    assert_eq!((written, deferred), (0, 1));
    let (written, deferred) = fake.indexed_db_write("pb", &stage, records.clone());
    assert_eq!((written, deferred), (1, 0));
    // A name that leads out of the folder is refused whole.
    let mut bad = records.clone();
    if let Some(Record::IndexedDb {
        value: Some(value), ..
    }) = bad.first_mut()
    {
        value.files[0].path = "leveldb/../../x".into();
    }
    fake.send(Command::IndexedDbWrite {
        req: 900,
        dir: "pc".into(),
        stage: stage.clone(),
        records: bad,
    });
    let error = fake.wait(Duration::from_secs(20), |e| match e {
        Event::IndexedDbWritten {
            req: 900, error, ..
        } => Some(error.clone()),
        _ => None,
    });
    assert!(error.is_some());
    assert!(!fake.root.join("pc/IndexedDB").join("x").exists());

    fake.open(2, profile("b", Some("pb")), 200, 100, 1.0);
    fake.go(2, "http://site.test/idb-read");
    fake.title_is(2, "idb=hello");
    fake.send(Command::StageDrop {
        stage: stage.clone(),
    });
    wait_until("the stage to go", || !fake.root.join(&stage).exists());
    assert_eq!(fake.quit(), Some(0));
}

#[test]
#[ignore]
fn a_watched_profile_reports_its_indexed_db_once_it_lies_still() {
    let mut fake = Fake::start();
    fake.proxy.respond(profile_site);
    fake.open(1, profile("a", Some("pa")), 200, 100, 1.0);
    fake.go(1, "http://site.test/");
    fake.profile_watch(1, true);
    std::thread::sleep(Duration::from_secs(2));
    let mark = fake.mark();
    fake.go(1, "http://site.test/idb-write");
    fake.title_is(1, "idb-written");
    let (changes, stage) = fake
        .changed_since(mark, Duration::from_secs(15), |r| {
            matches!(r, Record::IndexedDb { value: Some(_), .. })
        })
        .unwrap_or_else(|| panic!("the folder was not reported: {}", fake.recent(8)));
    let stage = stage.expect("a stage");
    let Some(Record::IndexedDb {
        origin,
        value: Some(value),
    }) = changes
        .iter()
        .find(|r| matches!(r, Record::IndexedDb { .. }))
    else {
        panic!("no folder")
    };
    assert!(
        fake.root
            .join(&stage)
            .join(origin)
            .join(&value.files[0].path)
            .exists()
    );
    assert_eq!(fake.quit(), Some(0));
}
