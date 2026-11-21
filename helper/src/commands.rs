//! What bateri asks of the helper, and the life of a browser: opening it in its profile's context,
//! applying commands to it, closing it, and taking everything down in order at the end.
//!
//! Every function here runs on the main thread, outside any borrow of the state: CEF is called
//! only after the state has been let go (see `state`).

use crate::convert::{key_event_of, mods_to_cef};
use crate::handlers::{HelperClient, observe, push_state};
use crate::state::{
    self, Context, Creds, Entry, FindStart, browser_of, emit, emit_error, with, with_entry,
};
use crate::{agent, rules, trace};
use browser_ipc::Command;
use browser_ipc::message::{Button, Event, MouseKind, Profile};
use cef::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Commands kept for a browser the engine has not finished creating.
const QUEUE_LIMIT: usize = 256;

pub fn handle(command: Command) {
    match command {
        Command::Quit => state::request_quit(),
        Command::Open {
            browser,
            profile,
            width,
            height,
            scale,
        } => open(browser, profile, width, height, scale),
        Command::Close { browser } => close(browser),
        Command::MenuResult { req, command } => {
            let Some((_, callback)) = with(|s| s.pending.menus.remove(&req)) else {
                return;
            };
            match command {
                Some(id) => callback.cont(id, EventFlags::from(sys::cef_event_flags_t(0))),
                None => callback.cancel(),
            }
        }
        Command::DialogResult { req, ok, text } => {
            if let Some((_, callback)) = with(|s| s.pending.dialogs.remove(&req)) {
                callback.cont(ok as _, Some(&text.as_str().into()));
            }
        }
        Command::FileResult { req, paths } => {
            let Some((_, callback)) = with(|s| s.pending.files.remove(&req)) else {
                return;
            };
            if paths.is_empty() {
                callback.cancel();
            } else {
                let mut list = CefStringList::new();
                for path in &paths {
                    list.append(path);
                }
                callback.cont(Some(&mut list));
            }
        }
        Command::DownloadResult { req, path } => download_result(req, path),
        Command::DownloadCancel { req } => download_cancel(req),
        other => {
            // Everything left names a browser.
            let Some(id) = other.browser() else { return };
            let exists = with(|s| s.entries.contains_key(&id));
            if !exists {
                emit_error(Some(id), format!("no such browser: {}", other.name()));
                return;
            }
            match browser_of(id) {
                Some(browser) => apply(id, &browser, other),
                None => {
                    let full = with_entry(id, |e| {
                        if e.queue.len() >= QUEUE_LIMIT {
                            true
                        } else {
                            e.queue.push(other);
                            false
                        }
                    });
                    if full == Some(true) {
                        emit_error(Some(id), "too many commands before the browser was ready");
                    }
                }
            }
        }
    }
}

fn open(id: u32, profile: Profile, width: u32, height: u32, scale: f32) {
    if with(|s| s.entries.contains_key(&id)) {
        emit_error(Some(id), "that browser number is in use");
        return;
    }
    if let Some(dir) = &profile.dir
        && !rules::profile_dir_ok(dir)
    {
        emit_error(Some(id), format!("not a profile folder name: {dir:?}"));
        return;
    }
    let (w, h, scale) = rules::clamp_view(width, height, scale);
    enum Context_ {
        Ready,
        Waiting,
        Mismatch,
        New,
    }
    let known = with(|s| {
        if let Some(c) = s.contexts.get_mut(&profile.context) {
            if c.dir != profile.dir || c.creds.user != profile.user || c.creds.pass != profile.pass
            {
                return Context_::Mismatch;
            }
            c.users += 1;
            if c.initialized {
                Context_::Ready
            } else {
                c.waiting.push(id);
                Context_::Waiting
            }
        } else if profile.dir.is_some() && s.contexts.values().any(|c| c.dir == profile.dir) {
            // Two request contexts on one folder would fight over it.
            Context_::Mismatch
        } else {
            Context_::New
        }
    });
    if matches!(known, Context_::Mismatch) {
        emit_error(
            Some(id),
            format!(
                "profile {:?} was opened with other settings",
                profile.context
            ),
        );
        return;
    }
    with(|s| {
        s.entries.insert(
            id,
            Entry {
                id,
                context: profile.context.clone(),
                size: (w, h),
                scale,
                ..Default::default()
            },
        )
    });
    match known {
        Context_::Ready => create_browser(id),
        Context_::Waiting | Context_::Mismatch => {}
        Context_::New => {
            if !create_context(&profile) {
                with(|s| s.entries.remove(&id));
                emit_error(Some(id), "the profile could not be created");
                return;
            }
            with(|s| {
                if let Some(c) = s.contexts.get_mut(&profile.context) {
                    c.waiting.push(id);
                    c.users = 1;
                }
            });
            if with(|s| s.early.remove(&profile.context)) {
                context_ready(&profile.context);
            }
        }
    }
}

/// Creates the profile's request context; the engine says when it is ready (`context_ready`).
fn create_context(profile: &Profile) -> bool {
    let cache_path = match &profile.dir {
        None => String::new(),
        Some(dir) => {
            let path = with(|s| s.root.join(dir));
            // A folder the helper did not make, or a link out of the root, is not a profile.
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                return false;
            }
            path.to_string_lossy().into_owned()
        }
    };
    let settings = RequestContextSettings {
        cache_path: cache_path.as_str().into(),
        ..Default::default()
    };
    let mut handler = ContextHandler::new(profile.context.clone());
    let Some(context) = request_context_create_context(Some(&settings), Some(&mut handler)) else {
        return false;
    };
    with(|s| {
        s.contexts.insert(
            profile.context.clone(),
            Context {
                context,
                creds: Arc::new(Creds::new(profile.user.clone(), profile.pass.clone())),
                dir: profile.dir.clone(),
                initialized: false,
                users: 0,
                waiting: Vec::new(),
            },
        )
    });
    true
}

wrap_request_context_handler! {
    struct ContextHandler {
        key: String,
    }

    impl RequestContextHandler {
        fn on_request_context_initialized(&self, _context: Option<&mut RequestContext>) {
            // The engine may say so before `create_context` has put the context away.
            if with(|s| s.contexts.contains_key(&self.key)) {
                context_ready(&self.key);
            } else {
                with(|s| s.early.insert(self.key.clone()));
            }
        }
    }
}

fn context_ready(key: &str) {
    let waiting = with(|s| {
        s.contexts.get_mut(key).map(|c| {
            c.initialized = true;
            std::mem::take(&mut c.waiting)
        })
    })
    .unwrap_or_default();
    for id in waiting {
        match with_entry(id, |e| e.closing) {
            Some(false) => create_browser(id),
            Some(true) => closed(id),
            None => {}
        }
    }
}

fn create_browser(id: u32) {
    let found = with(|s| {
        let entry = s.entries.get(&id)?;
        let context = s.contexts.get(&entry.context)?;
        Some((context.context.clone(), context.creds.clone()))
    });
    let Some((mut context, creds)) = found else {
        return;
    };
    let window_info = WindowInfo {
        windowless_rendering_enabled: 1,
        shared_texture_enabled: 1,
        runtime_style: RuntimeStyle::ALLOY,
        ..Default::default()
    };
    let settings = BrowserSettings {
        windowless_frame_rate: 60,
        // Opaque white: what Chromium fills with while a resize waits for the page is this colour
        // (black when it is left transparent), and a page that paints no background is white.
        background_color: 0xFFFF_FFFF,
        ..Default::default()
    };
    let mut client = HelperClient::new(id, creds);
    let ok = browser_host_create_browser(
        Some(&window_info),
        Some(&mut client),
        Some(&"about:blank".into()),
        Some(&settings),
        None,
        Some(&mut context),
    );
    if ok != 1 {
        with(|s| s.entries.remove(&id));
        emit_error(Some(id), "the engine would not create a browser");
    }
}

/// The engine made the browser (`on_after_created`).
pub fn created(id: u32, browser: Browser) {
    let Some(closing) = with_entry(id, |e| {
        e.browser = Some(browser.clone());
        e.closing
    }) else {
        // It was closed before it ever came up: nobody is waiting for it.
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        return;
    };
    if closing {
        if let Some(host) = browser.host() {
            host.close_browser(1);
        }
        return;
    }
    let observer = observe(id, &browser);
    let queued = with_entry(id, |e| {
        e.observer = observer;
        std::mem::take(&mut e.queue)
    })
    .unwrap_or_default();
    emit(Event::Opened { browser: id });
    push_state(id);
    for command in queued {
        apply(id, &browser, command);
    }
}

/// The browser is gone (`on_before_close`).
pub fn closed(id: u32) {
    agent::fail_all(id, "the browser was closed");
    dismiss_all(id);
    let entry = with(|s| {
        let entry = s.entries.remove(&id);
        if let Some(e) = &entry
            && let Some(c) = s.contexts.get_mut(&e.context)
        {
            c.users = c.users.saturating_sub(1);
        }
        entry
    });
    // Released out of the borrow: letting go of the engine's handles may call back.
    drop(entry);
    emit(Event::Closed { browser: id });
}

fn close(id: u32) {
    let found = with_entry(id, |e| {
        e.closing = true;
        e.browser.clone()
    });
    match found {
        None => emit_error(Some(id), "no such browser: close"),
        // Not up yet: `created` or `context_ready` sees the flag.
        Some(None) => {}
        Some(Some(browser)) => {
            if let Some(host) = browser.host() {
                host.close_browser(1);
            }
        }
    }
}

/// Everything a browser had open for bateri to answer is answered "no": the page it belonged to
/// is gone, so nobody is waiting for it.
pub fn dismiss_all(id: u32) {
    let (menus, dialogs, files, downloads) = with(|s| {
        let menus: Vec<u32> = s
            .pending
            .menus
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| *k)
            .collect();
        let dialogs: Vec<u32> = s
            .pending
            .dialogs
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| *k)
            .collect();
        let files: Vec<u32> = s
            .pending
            .files
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| *k)
            .collect();
        let downloads: Vec<u32> = s
            .pending
            .before_download
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| *k)
            .collect();
        (menus, dialogs, files, downloads)
    });
    for req in menus {
        if let Some((_, callback)) = with(|s| s.pending.menus.remove(&req)) {
            callback.cancel();
        }
    }
    for req in dialogs {
        if let Some((_, callback)) = with(|s| s.pending.dialogs.remove(&req)) {
            callback.cont(0, Some(&"".into()));
        }
    }
    for req in files {
        if let Some((_, callback)) = with(|s| s.pending.files.remove(&req)) {
            callback.cancel();
        }
    }
    for req in downloads {
        download_result(req, None);
    }
}

/// The page moved on: its open dialogs are void, the engine has already dropped them.
pub fn dismiss_dialogs(id: u32) {
    with(|s| s.pending.dialogs.retain(|_, v| v.0 != id));
}

fn download_result(req: u32, path: Option<String>) {
    let Some((_, callback)) = with(|s| s.pending.before_download.remove(&req)) else {
        return;
    };
    match path {
        Some(path) => callback.cont(Some(&path.as_str().into()), 0),
        None => {
            // A callback let go of unanswered ends the download.
            drop(callback);
            with(|s| s.pending.downloads.retain(|_, v| v.0 != req));
            emit(Event::DownloadProgress {
                req,
                state: browser_ipc::message::DownloadState::Canceled,
                received: 0,
                total: None,
            });
        }
    }
}

fn download_cancel(req: u32) {
    if with(|s| s.pending.before_download.contains_key(&req)) {
        download_result(req, None);
        return;
    }
    let callback = with(|s| {
        s.pending
            .downloads
            .values()
            .find(|v| v.0 == req)
            .and_then(|v| v.1.clone())
    });
    if let Some(callback) = callback {
        callback.cancel();
    }
}

/// A command for a browser that exists and is up.
fn apply(id: u32, browser: &Browser, command: Command) {
    let Some(host) = browser.host() else { return };
    match command {
        Command::Navigate { url, .. } => match rules::check_address(&url) {
            Ok(()) => {
                if let Some(frame) = browser.main_frame() {
                    frame.load_url(Some(&url.as_str().into()));
                }
            }
            Err(why) => emit(Event::Refused {
                browser: id,
                url,
                reason: why.reason().into(),
            }),
        },
        Command::Back { .. } => browser.go_back(),
        Command::Forward { .. } => browser.go_forward(),
        Command::Reload { ignore_cache, .. } => {
            if ignore_cache {
                browser.reload_ignore_cache();
            } else {
                browser.reload();
            }
        }
        Command::Stop { .. } => browser.stop_load(),
        Command::Resize {
            width,
            height,
            scale,
            ..
        } => {
            let (w, h, scale) = rules::clamp_view(width, height, scale);
            with_entry(id, |e| {
                e.size = (w, h);
                e.scale = scale;
            });
            host.notify_screen_info_changed();
            host.was_resized();
        }
        Command::Focus { on, .. } => host.set_focus(on as _),
        Command::Mouse {
            kind,
            x,
            y,
            button,
            clicks,
            mods,
            ..
        } => {
            let event = MouseEvent {
                x,
                y,
                modifiers: mods_to_cef(mods),
            };
            let button = match button {
                Button::Left => MouseButtonType::LEFT,
                Button::Middle => MouseButtonType::MIDDLE,
                Button::Right => MouseButtonType::RIGHT,
            };
            let clicks = clicks.clamp(1, 3) as i32;
            match kind {
                MouseKind::Move => host.send_mouse_move_event(Some(&event), 0),
                MouseKind::Leave => host.send_mouse_move_event(Some(&event), 1),
                MouseKind::Down => host.send_mouse_click_event(Some(&event), button, 0, clicks),
                MouseKind::Up => host.send_mouse_click_event(Some(&event), button, 1, clicks),
            }
        }
        Command::Wheel {
            x, y, dx, dy, mods, ..
        } => {
            let event = MouseEvent {
                x,
                y,
                modifiers: mods_to_cef(mods),
            };
            host.send_mouse_wheel_event(Some(&event), dx, dy);
        }
        Command::Key { kind, key, .. } => {
            host.send_key_event(Some(&key_event_of(kind, &key)));
        }
        // Text the way the system's input method hands it over: committed as a whole, never as key
        // events (a Turkish letter has no key of its own), and composition while it is marked.
        Command::ImeCommit { text, .. } => {
            let any = Range {
                from: u32::MAX,
                to: u32::MAX,
            };
            host.ime_commit_text(Some(&text.as_str().into()), Some(&any), 0);
        }
        Command::ImeCompose { text, from, to, .. } => {
            let any = Range {
                from: u32::MAX,
                to: u32::MAX,
            };
            let selection = Range { from, to };
            host.ime_set_composition(
                Some(&text.as_str().into()),
                None,
                Some(&any),
                Some(&selection),
            );
        }
        Command::ImeCancel { .. } => host.ime_cancel_composition(),
        Command::Find {
            pattern,
            case,
            forward,
            next,
            ..
        } => {
            if pattern.is_empty() {
                host.stop_finding(1);
                with_entry(id, |e| {
                    e.found = None;
                    e.find_activate = None;
                });
                push_state(id);
            } else {
                // A new search only counts; its first match is selected by `on_find_result`.
                let first = (!next).then(|| FindStart {
                    pattern: pattern.clone(),
                    forward,
                    case,
                    asked: false,
                });
                with_entry(id, |e| e.find_activate = first);
                host.find(
                    Some(&pattern.as_str().into()),
                    forward as _,
                    case as _,
                    next as _,
                );
            }
        }
        Command::StopFinding { keep_selection, .. } => {
            host.stop_finding(!keep_selection as _);
            with_entry(id, |e| {
                e.found = None;
                e.find_activate = None;
            });
            push_state(id);
        }
        Command::Zoom { ratio, .. } => {
            host.set_zoom_level(rules::ratio_to_level(ratio));
            push_state(id);
        }
        Command::DevTools { .. } => {
            if host.has_dev_tools() == 0 {
                host.show_dev_tools(
                    Some(&WindowInfo::default()),
                    None,
                    Some(&BrowserSettings::default()),
                    None,
                );
            } else {
                host.close_dev_tools();
            }
        }
        Command::Agent { req, op, .. } => agent::start(id, req, op),
        other => trace!("not for a browser: {}", other.name()),
    }
}

/// When the next dropped frame is to be asked for again.
pub fn next_repaint() -> Option<Instant> {
    with(|s| s.entries.values().filter_map(|e| e.repaint_at).min())
}

/// Asks for the frames that were dropped while bateri was behind.
pub fn tick() {
    let now = Instant::now();
    let due: Vec<u32> = with(|s| {
        s.entries
            .values_mut()
            .filter(|e| e.repaint_at.is_some_and(|t| t <= now))
            .map(|e| {
                e.repaint_at = None;
                e.id
            })
            .collect()
    });
    for id in due {
        if let Some(host) = browser_of(id).and_then(|b| b.host()) {
            host.invalidate(PaintElementType::default());
        }
    }
}

wrap_completion_callback! {
    struct Flushed {
        done: Arc<AtomicBool>,
    }

    impl CompletionCallback {
        fn on_complete(&self) {
            self.done.store(true, Ordering::SeqCst);
        }
    }
}

/// The first half of the end: every browser is told to close and every profile that lives on disk
/// writes its cookies out. Returns the flags the flushes set when they are done.
pub fn begin_shutdown() -> Vec<Arc<AtomicBool>> {
    let contexts: Vec<RequestContext> = with(|s| {
        s.contexts
            .values()
            .filter(|c| c.dir.is_some())
            .map(|c| c.context.clone())
            .collect()
    });
    let mut flags = Vec::new();
    for context in contexts {
        let done = Arc::new(AtomicBool::new(false));
        let mut callback = Flushed::new(done.clone());
        match context.cookie_manager(None) {
            Some(manager) if manager.flush_store(Some(&mut callback)) == 1 => flags.push(done),
            _ => {}
        }
    }
    let browsers: Vec<(u32, Option<Browser>)> = with(|s| {
        s.entries
            .values_mut()
            .map(|e| {
                e.closing = true;
                (e.id, e.browser.clone())
            })
            .collect()
    });
    for (id, browser) in browsers {
        match browser.and_then(|b| b.host()) {
            Some(host) => host.close_browser(1),
            // Never came up: nothing will close it.
            None => closed(id),
        }
    }
    flags
}

/// Every browser is closed.
pub fn none_open() -> bool {
    with(|s| s.entries.is_empty())
}

/// The profiles' request contexts and the rest the engine must not outlive.
pub fn release_all() {
    let contexts = with(|s| std::mem::take(&mut s.contexts));
    drop(contexts);
}
