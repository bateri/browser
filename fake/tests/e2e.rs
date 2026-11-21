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

#[test]
#[ignore]
fn command_arrows_come_back_with_nothing_focused_and_in_a_field() {
    let mut fake = Fake::start();
    serve(
        &fake,
        "<title>t</title><body style='margin:0'><p>text</p>\
         <input id=i style='position:absolute;left:10px;top:100px;width:200px;height:30px' value='hello'>\
         <script>addEventListener('keydown',e=>{document.title='k:'+e.key+':'+e.metaKey})</script>",
    );
    fake.open(1, profile("a", None), 400, 300, 1.0);
    fake.go(1, "http://page.test/");
    fake.send(Command::Focus {
        browser: 1,
        on: true,
    });
    let left = |fake: &Fake| tap(fake, 1, 37, 123, 0xF702, modifier::COMMAND);
    let right = |fake: &Fake| tap(fake, 1, 39, 124, 0xF703, modifier::COMMAND);
    // Nothing focused: the page's script sees it, nobody uses it, it comes back.
    left(&fake);
    fake.title_is(1, "k:ArrowLeft:true");
    right(&fake);
    fake.title_is(1, "k:ArrowRight:true");
    let downs = |fake: &Fake, vk: i32| {
        fake.history()
            .iter()
            .filter(|e| matches!(e, Event::UnhandledKey { kind: KeyKind::Down, key, .. } if key.vk == vk && key.mods == modifier::COMMAND))
            .count()
    };
    // A sentinel after them, so everything before has been told.
    tap(&fake, 1, 67, 8, 99, 0);
    fake.wait(SHORT, |e| match e {
        Event::UnhandledKey {
            kind: KeyKind::Up,
            key,
            ..
        } if key.vk == 67 => Some(()),
        _ => None,
    });
    assert_eq!((downs(&fake, 37), downs(&fake, 39)), (1, 1));
    // With the caret in a field, where ⌘← would be an editing command: the off-screen engine does
    // not use it either, so it comes back too (bateri cannot tell a field has the caret from
    // this, only from what it knows of the page).
    fake.click(1, 100, 115);
    left(&fake);
    right(&fake);
    tap(&fake, 1, 68, 2, 100, 0);
    fake.wait(SHORT, |e| match e {
        Event::UnhandledKey {
            kind: KeyKind::Up,
            key,
            ..
        } if key.vk == 68 => Some(()),
        _ => None,
    });
    assert_eq!((downs(&fake, 37), downs(&fake, 39)), (2, 2));
}
