//! What bateri asks of the helper, and the life of a browser: opening it in its profile's context,
//! applying commands to it, closing it, and taking everything down in order at the end.
//!
//! Every function here runs on the main thread, outside any borrow of the state: CEF is called
//! only after the state has been let go (see `state`).

use crate::convert::{key_event_of, mods_to_cef};
use crate::handlers::{HelperClient, ToolsClient, observe, push_state};
use crate::state::{
    self, Context, Creds, Entry, FindStart, Fired, OpenMenu, State, browser_of, emit, emit_error,
    take_where, with, with_entry,
};
use crate::{agent, clear, keys, rules, switches, sync, trace};
use browser_ipc::Command;
use browser_ipc::message::{Button, DownloadState, EditAction, Event, KeyKind, MouseKind, Profile};
use cef::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Commands kept for a browser the engine has not finished creating.
const QUEUE_LIMIT: usize = 256;

/// How long after the developer tools are asked for their window is brought to the front.
const DEVTOOLS_FRONT: std::time::Duration = std::time::Duration::from_secs(3);

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
            let Some((
                browser,
                OpenMenu {
                    callback,
                    offered,
                    at,
                    urls,
                },
            )) = with(|s| s.pending.menus.remove(&req))
            else {
                return;
            };
            match command {
                // Only a command the menu held is run: the engine has more behind its other
                // numbers (`view-source:` pages among them) than the helper ever loads.
                //
                // The rows the helper added are its own to run: the engine does not know them.
                Some(id) if rules::menu_command_offered(&offered, id) => {
                    match rules::OwnRow::of(id) {
                        Some(row) => {
                            callback.cancel();
                            own_row(browser, row, at, &urls);
                        }
                        None => callback.cont(id, EventFlags::from(sys::cef_event_flags_t(0))),
                    }
                }
                Some(id) => {
                    trace!("menu {req}: command {id} was not offered");
                    callback.cancel();
                }
                None => callback.cancel(),
            }
        }
        Command::DialogResult { req, ok, text } => {
            if let Some((_, _, callback)) = with(|s| s.pending.dialogs.remove(&req)) {
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
        Command::IndexedDbRead { req, dir } => sync::idb_read(req, dir),
        Command::IndexedDbWrite {
            req,
            dir,
            stage,
            records,
        } => sync::idb_write(req, dir, stage, records),
        Command::StageDrop { stage } => sync::stage_drop(&stage),
        other => {
            // Everything left names a browser.
            let Some(id) = other.browser() else { return };
            let exists = with(|s| s.entries.contains_key(&id));
            if !exists {
                // A job is answered; anything else is an error of the conversation.
                if !agent::refuse(id, &other, "no such browser") {
                    emit_error(Some(id), format!("no such browser: {}", other.name()));
                }
                return;
            }
            match browser_of(id) {
                Some(browser) => apply(id, &browser, other),
                None => {
                    // The command is kept, or handed back when the queue is full.
                    let turned_away =
                        with_entry(id, |e| keep_or_turn_away(&mut e.queue, other, QUEUE_LIMIT))
                            .flatten();
                    if let Some(command) = turned_away {
                        let why = "too many commands before the browser was ready";
                        if !agent::refuse(id, &command, why) {
                            emit_error(Some(id), why);
                        }
                    }
                }
            }
        }
    }
}

/// Puts a command that came before its browser was ready in the queue, or hands it back when the
/// queue is full (the caller answers it: a job's sender is waiting for one).
fn keep_or_turn_away(queue: &mut Vec<Command>, command: Command, limit: usize) -> Option<Command> {
    if queue.len() >= limit {
        Some(command)
    } else {
        queue.push(command);
        None
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
        Failed(String),
        New,
    }
    let known = with(|s| {
        if let Some(c) = s.contexts.get_mut(&profile.context) {
            if c.dir != profile.dir || c.creds.user != profile.user || c.creds.pass != profile.pass
            {
                return Context_::Mismatch;
            }
            if let Some(why) = &c.failed {
                return Context_::Failed(why.clone());
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
    if let Context_::Failed(why) = known {
        emit_error(Some(id), why);
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
        Context_::Waiting | Context_::Mismatch | Context_::Failed(_) => {}
        Context_::New => {
            if !create_context(&profile) {
                drop(drop_entry(id, "the profile could not be created"));
                emit_error(Some(id), "the profile could not be created");
                return;
            }
            with(|s| {
                if let Some(c) = s.contexts.get_mut(&profile.context) {
                    c.waiting.push(id);
                    c.users = 1;
                }
            });
            if let Some(failure) = with(|s| s.early.remove(&profile.context)) {
                context_ready(&profile.context, failure);
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
    // A profile that has a name is kept on disk, and what it keeps is what its home keeps for it
    // (`sync`): the cookies of the session stay too, or a restart would take from the profile what
    // the home still holds and nothing would give it back. The engine ignores the setting where
    // there is no folder.
    let settings = RequestContextSettings {
        cache_path: cache_path.as_str().into(),
        persist_session_cookies: i32::from(profile.dir.is_some()),
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
                failed: None,
                creds: Arc::new(Creds::new(profile.user.clone(), profile.pass.clone())),
                dir: profile.dir.clone(),
                initialized: false,
                users: 0,
                waiting: Vec::new(),
                accepted: Default::default(),
                origins: Default::default(),
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
        fn on_request_context_initialized(&self, context: Option<&mut RequestContext>) {
            // A context the network is not closed in is not used (see `harden`).
            let failure = match context {
                Some(context) => harden(context).err(),
                None => Some("the engine gave no request context".to_string()),
            };
            // The engine may say so before `create_context` has put the context away.
            if with(|s| s.contexts.contains_key(&self.key)) {
                context_ready(&self.key, failure);
            } else {
                with(|s| s.early.insert(self.key.clone(), failure));
            }
        }
    }
}

/// Holds a request context to the preferences `switches::PREFS` names (WebRTC's UDP only through
/// the proxy, no DNS prefetching) and reads each back: what counts is the value in force. The
/// first is also on the command line, which decides it (a preference the command line decides
/// refuses to be set, and is right); the second is only here. A value that is not in force is an
/// error of the context, and no browser is opened in it.
fn harden(context: &RequestContext) -> Result<(), String> {
    for (name, want) in switches::PREFS {
        let key: CefString = (*name).into();
        if context.has_preference(Some(&key)) == 0 {
            return Err(format!("the engine has no preference {name}"));
        }
        if pref_is(context, &key, *want) {
            continue;
        }
        let Some(mut value) = value_create() else {
            return Err("the engine made no value".into());
        };
        match want {
            switches::Pref::Str(text) => value.set_string(Some(&(*text).into())),
            switches::Pref::Int(number) => value.set_int(*number),
        };
        // A non-empty `error`: cef-rs passes an empty string as a null pointer and the engine then
        // fails the call without a word.
        let mut error: CefString = "-".into();
        context.set_preference(Some(&key), Some(&mut value), Some(&mut error));
        if !pref_is(context, &key, *want) {
            return Err(format!(
                "preference {name} is not {want:?} in this engine ({error})"
            ));
        }
    }
    Ok(())
}

fn pref_is(context: &RequestContext, key: &CefString, want: switches::Pref) -> bool {
    let Some(value) = context.preference(Some(key)) else {
        return false;
    };
    match want {
        switches::Pref::Str(text) => CefString::from(&value.string()).to_string() == text,
        switches::Pref::Int(number) => value.int() == number,
    }
}

fn context_ready(key: &str, failure: Option<String>) {
    let waiting = with(|s| {
        s.contexts.get_mut(key).map(|c| {
            c.initialized = true;
            c.failed = failure.clone();
            std::mem::take(&mut c.waiting)
        })
    })
    .unwrap_or_default();
    for id in waiting {
        match (with_entry(id, |e| e.closing), &failure) {
            (Some(true), _) => closed(id),
            (Some(false), Some(why)) => {
                drop(drop_entry(id, why));
                emit_error(Some(id), why.clone());
            }
            (Some(false), None) => create_browser(id),
            (None, _) => {}
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
        drop(drop_entry(id, "the engine would not create a browser"));
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
    crate::console::enable(id);
    sync::browser_ready(id);
    for command in queued {
        apply(id, &browser, command);
    }
}

/// Takes a browser's entry out of the state: its jobs still waiting for the browser are answered
/// (they were kept for it, and nobody else will), and its profile has one user less. The entry
/// comes back to be dropped by the caller, outside the borrow: letting go of the engine's handles
/// may call back.
fn drop_entry(id: u32, why: &str) -> Option<Entry> {
    let entry = with(|s| {
        let entry = s.entries.remove(&id);
        if let Some(e) = &entry
            && let Some(c) = s.contexts.get_mut(&e.context)
        {
            c.users = c.users.saturating_sub(1);
        }
        entry
    });
    if let Some(e) = &entry {
        agent::refuse_all(id, &e.queue, why);
    }
    entry
}

/// The browser is gone (`on_before_close`).
pub fn closed(id: u32) {
    sync::browser_gone(id);
    agent::fail_all(id, "the browser was closed");
    dismiss_all(id);
    cancel_downloads(id);
    // Released out of the borrow: letting go of the engine's handles may call back.
    drop(drop_entry(id, "the browser was closed"));
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
            // A question the page is stopped at is answered first: the browser does not close
            // under one it is waiting on.
            answer_for_close(id);
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
        let menus = take_where(&mut s.pending.menus, |_, v| v.0 == id);
        let dialogs = take_where(&mut s.pending.dialogs, |_, v| v.0 == id);
        let files = take_where(&mut s.pending.files, |_, v| v.0 == id);
        let downloads: Vec<u32> = s
            .pending
            .before_download
            .iter()
            .filter(|(_, v)| v.0 == id)
            .map(|(k, _)| *k)
            .collect();
        (menus, dialogs, files, downloads)
    });
    // Outside the borrow from here on: each answer may call back into a handler.
    for (_, (_, menu)) in menus {
        menu.callback.cancel();
    }
    for (_, (_, kind, callback)) in dialogs {
        callback.cont(rules::close_answer(kind) as _, Some(&"".into()));
    }
    for (_, (_, callback)) in files {
        callback.cancel();
    }
    for req in downloads {
        download_result(req, None);
    }
}

/// The browser is about to close (or the helper is): the questions the page is stopped at are
/// answered, the "leave this page?" one with yes, so closing is not left waiting on them.
fn answer_for_close(id: u32) {
    let dialogs = with(|s| take_where(&mut s.pending.dialogs, |_, v| v.0 == id));
    for (_, (_, kind, callback)) in dialogs {
        callback.cont(rules::close_answer(kind) as _, Some(&"".into()));
    }
}

/// The page moved on: its open dialogs are void, the engine has already dropped them. bateri is
/// told, so the question it shows goes away too.
pub fn dismiss_dialogs(id: u32) {
    let gone = with(|s| take_where(&mut s.pending.dialogs, |_, v| v.0 == id));
    let mut reqs: Vec<u32> = gone.iter().map(|(req, _)| *req).collect();
    // Let go of the engine's handles first, outside the borrow.
    drop(gone);
    reqs.sort_unstable();
    for req in reqs {
        emit(Event::DialogVoid { browser: id, req });
    }
}

/// A browser's downloads that are still running end with it: the engine's manager belongs to the
/// profile and would go on, with nobody to hear of it. Each is cancelled and bateri is told once.
fn cancel_downloads(id: u32) {
    let running = with(|s| take_where(&mut s.pending.downloads, |k, _| k.0 == id));
    for (_, download) in running {
        if let Some(callback) = &download.cancel {
            callback.cancel();
        }
        emit(Event::DownloadProgress {
            req: download.req,
            state: DownloadState::Canceled,
            received: 0,
            total: None,
        });
    }
}

fn download_result(req: u32, path: Option<String>) {
    let Some((_, callback)) = with(|s| s.pending.before_download.remove(&req)) else {
        return;
    };
    match path {
        Some(path) => callback.cont(Some(&path.as_str().into()), 0),
        None => {
            // The record goes first: a callback let go of unanswered ends the download, and the
            // engine's own word about that (an update) must find nothing to report a second time.
            let ended = with(|s| take_where(&mut s.pending.downloads, |_, d| d.req == req));
            drop(callback);
            drop(ended);
            emit(Event::DownloadProgress {
                req,
                state: DownloadState::Canceled,
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
            .find(|d| d.req == req)
            .and_then(|d| d.cancel.clone())
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
                Button::Back | Button::Forward => {
                    let ratio = rules::level_to_ratio(host.zoom_level());
                    let call = rules::side_button_call(
                        kind,
                        button == Button::Back,
                        f64::from(x) / ratio,
                        f64::from(y) / ratio,
                    );
                    if let Some(params) = call {
                        agent::call(id, "Input.dispatchMouseEvent", params, |_| {});
                    }
                    return;
                }
            };
            let clicks = clicks.clamp(1, 3) as i32;
            crate::mac::input_now(|| match kind {
                MouseKind::Move => host.send_mouse_move_event(Some(&event), 0),
                MouseKind::Leave => host.send_mouse_move_event(Some(&event), 1),
                MouseKind::Down => host.send_mouse_click_event(Some(&event), button, 0, clicks),
                MouseKind::Up => host.send_mouse_click_event(Some(&event), button, 1, clicks),
            });
        }
        Command::CanScroll { req, x, y, dx, .. } => crate::scroll::ask(id, req, x, y, dx),
        Command::Windows { mode, .. } => {
            with_entry(id, |e| e.new_windows = mode);
        }
        Command::Pinch {
            kind, x, y, scale, ..
        } => crate::pinch::apply(id, kind, x, y, scale),
        Command::Wheel {
            x, y, dx, dy, mods, ..
        } => {
            let event = MouseEvent {
                x,
                y,
                modifiers: mods_to_cef(mods),
            };
            crate::mac::input_now(|| host.send_mouse_wheel_event(Some(&event), dx, dy));
        }
        // A key with editing commands goes through the protocol, which can carry them to the page
        // (the engine's own key call cannot); the page's default for the key is then the commands,
        // and a key nobody uses still comes back as `UnhandledKey`. Everything else, and a key
        // whose commands were all dropped, is the engine's plain key event: both paths reach the
        // page in the order they were sent (a test sends hundreds of mixed keys back to back).
        Command::Key {
            kind,
            key,
            commands,
            ..
        } => {
            let commands = if kind == KeyKind::Down {
                keys::clean_commands(&commands)
            } else {
                vec![]
            };
            let sent = !commands.is_empty()
                && agent::fire(
                    id,
                    "Input.dispatchKeyEvent",
                    keys::dispatch_key_down(&key, &commands),
                    Fired { kind, key },
                );
            if !sent {
                crate::mac::input_now(|| host.send_key_event(Some(&key_event_of(kind, &key))));
            }
        }
        // The Edit menu: the frame that has the focus runs the command on what it has selected, and
        // copy, cut and paste use the system's pasteboard.
        Command::Edit { action, .. } => {
            if let Some(frame) = browser.focused_frame().or_else(|| browser.main_frame()) {
                match action {
                    EditAction::Copy => frame.copy(),
                    EditAction::Cut => frame.cut(),
                    EditAction::Paste => frame.paste(),
                    EditAction::Undo => frame.undo(),
                    EditAction::Redo => frame.redo(),
                    EditAction::SelectAll => frame.select_all(),
                }
            }
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
                show_tools(id, &host, None);
            } else {
                host.close_dev_tools();
            }
        }
        Command::Agent { req, op, .. } => agent::start(id, req, op),
        Command::Proceed { .. } => clear::proceed(id),
        Command::ClearCache { req, origin, .. } => clear::cache(id, req, origin),
        Command::ClearSiteData { req, host, .. } => clear::site(id, req, host),
        Command::ProfileRead { req, .. } => sync::read(id, req),
        Command::ProfileWrite { req, records, .. } => sync::write(id, req, records),
        Command::ProfileWatch { on, .. } => sync::watch(id, on),
        other => trace!("not for a browser: {}", other.name()),
    }
}

/// Opens the developer tools of a browser, looking at the element at `at` (points from the view's
/// corner) when there is one. Where they are open already the engine looks at the element in the
/// same window, which is brought forward.
fn show_tools(id: u32, host: &BrowserHost, at: Option<(i32, i32)>) {
    let open = host.has_dev_tools() != 0;
    // The windows there are now are not this request's (`front_new_windows`).
    crate::mac::note_existing_windows();
    let mut client = ToolsClient::new(id);
    match at {
        Some((x, y)) => trace!("the developer tools of browser {id} are asked for at {x},{y}"),
        None => trace!("the developer tools of browser {id} are asked for"),
    }
    let at = at.map(|(x, y)| Point { x, y });
    host.show_dev_tools(
        Some(&WindowInfo::default()),
        Some(&mut client),
        Some(&BrowserSettings::default()),
        at.as_ref(),
    );
    if open {
        // The window is there and was claimed above: it is brought forward here, not by the wait.
        crate::mac::front_open_windows();
    }
    with_entry(id, |e| {
        e.devtools_front = Some(Instant::now() + DEVTOOLS_FRONT)
    });
}

/// A row the helper added to the context menu was picked: run here, on the browser the menu was
/// asked for, at the point it was asked at and for the link and image it was asked for.
fn own_row(id: u32, row: rules::OwnRow, at: (i32, i32), urls: &rules::MenuUrls) {
    let Some(host) = browser_of(id).and_then(|b| b.host()) else {
        return;
    };
    let ratio = rules::level_to_ratio(host.zoom_level());
    let zoom = |to: Option<f64>| {
        if let Some(to) = to {
            host.set_zoom_level(rules::ratio_to_level(to));
            push_state(id);
        }
    };
    match row {
        rules::OwnRow::ZoomIn => zoom(rules::zoom_step(ratio, true)),
        rules::OwnRow::ZoomOut => zoom(rules::zoom_step(ratio, false)),
        rules::OwnRow::ActualSize => zoom(Some(1.0)),
        rules::OwnRow::Inspect => show_tools(id, &host, Some(at)),
        // What opens is asked of bateri as a window in the background, the way a page's own link
        // that wants one is (`rules::window_event` refuses an address the helper does not load);
        // what copies goes to the system pasteboard.
        rules::OwnRow::OpenLink | rules::OwnRow::OpenImage => {
            if let Some(url) = urls.of(row) {
                emit(rules::window_event(id, url, true));
            }
        }
        rules::OwnRow::CopyLink | rules::OwnRow::CopyImage => {
            if let Some(url) = urls.of(row)
                && !crate::mac::copy_text(url)
            {
                trace!("browser {id}: the address was not put on the pasteboard");
            }
        }
    }
}

/// The browsers still waiting for their developer-tools window, the oldest request first; a
/// request whose time ran out is let go.
fn front_waiters(s: &mut State, now: Instant) -> Vec<u32> {
    let mut waiting: Vec<(Instant, u32)> = Vec::new();
    for e in s.entries.values_mut() {
        match e.devtools_front {
            Some(until) if until <= now => e.devtools_front = None,
            Some(until) => waiting.push((until, e.id)),
            None => {}
        }
    }
    waiting.sort();
    waiting.into_iter().map(|(_, id)| id).collect()
}

/// `raised` windows were brought forward: that many of the oldest requests are done, and only
/// those (the others' windows are still to come).
fn fronted(s: &mut State, waiting: &[u32], raised: usize) {
    for id in waiting.iter().take(raised) {
        if let Some(e) = s.entries.get_mut(id) {
            e.devtools_front = None;
        }
    }
}

/// When the next dropped frame is to be asked for again.
pub fn next_repaint() -> Option<Instant> {
    with(|s| s.entries.values().filter_map(|e| e.repaint_at).min())
}

/// Asks for the frames that were dropped while bateri was behind.
pub fn tick() {
    sync::tick();
    let now = Instant::now();
    let waiting = with(|s| front_waiters(s, now));
    // The window exists only after the engine has made it, a turn or two after the command.
    if !waiting.is_empty() {
        let raised = crate::mac::front_new_windows();
        if raised > 0 {
            with(|s| fronted(s, &waiting, raised));
        }
    }
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
            Some(host) => {
                answer_for_close(id);
                host.close_browser(1);
            }
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

/// The browsers, the profiles' request contexts and everything else the engine must not outlive,
/// let go of in the order the engine needs and outside any borrow of the state: its handles may
/// call back when they go, and the engine is shut down right after. What is still open is
/// answered first (a browser that did not close in time had questions and downloads, and jobs
/// that wait for an answer bateri will not hear).
pub fn release_all() {
    let ids: Vec<u32> = with(|s| s.entries.keys().copied().collect());
    for id in ids {
        sync::browser_gone(id);
        agent::fail_all(id, "the helper is quitting");
        dismiss_all(id);
        cancel_downloads(id);
        if let Some(entry) = drop_entry(id, "the helper is quitting") {
            // Browsers (and what hangs on them) go before the contexts they live in.
            drop(entry);
        }
    }
    let (contexts, pending) = with(|s| {
        s.early.clear();
        (
            std::mem::take(&mut s.contexts),
            std::mem::take(&mut s.pending),
        )
    });
    drop(pending);
    drop(contexts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_queue_hands_the_command_back_and_keeps_the_ones_it_holds() {
        use browser_ipc::message::AgentOp;
        let job = |req| Command::Agent {
            browser: 1,
            req,
            op: AgentOp::Text,
        };
        let mut queue = Vec::new();
        assert!(keep_or_turn_away(&mut queue, job(1), 2).is_none());
        assert!(keep_or_turn_away(&mut queue, job(2), 2).is_none());
        let back = keep_or_turn_away(&mut queue, job(3), 2);
        assert!(
            matches!(back, Some(Command::Agent { req: 3, .. })),
            "{back:?}"
        );
        assert_eq!(queue.len(), 2);
    }

    fn entries(deadlines: &[(u32, Option<Instant>)]) -> State {
        let mut s = State::default();
        for (id, until) in deadlines {
            s.entries.insert(
                *id,
                Entry {
                    id: *id,
                    devtools_front: *until,
                    ..Default::default()
                },
            );
        }
        s
    }

    #[test]
    fn requests_for_the_tools_window_wait_oldest_first_and_expire_one_by_one() {
        let now = Instant::now();
        let secs = |n| now + std::time::Duration::from_secs(n);
        let mut s = entries(&[
            (1, Some(secs(5))),
            (2, Some(secs(3))),
            (3, None),
            (4, Some(now)),
        ]);
        // Entry 4's time ran out: it is let go, and the others are in the order they asked in.
        assert_eq!(front_waiters(&mut s, now), vec![2, 1]);
        assert_eq!(s.entries[&4].devtools_front, None);
        assert!(s.entries[&1].devtools_front.is_some() && s.entries[&2].devtools_front.is_some());
    }

    #[test]
    fn a_window_brought_forward_settles_one_request_not_all_of_them() {
        let now = Instant::now();
        let secs = |n| now + std::time::Duration::from_secs(n);
        let mut s = entries(&[(1, Some(secs(5))), (2, Some(secs(3))), (3, Some(secs(4)))]);
        let waiting = front_waiters(&mut s, now);
        assert_eq!(waiting, vec![2, 3, 1]);
        // One window came up: the oldest request has its window, the other two are still waiting.
        fronted(&mut s, &waiting, 1);
        assert_eq!(s.entries[&2].devtools_front, None);
        assert!(s.entries[&3].devtools_front.is_some());
        assert!(s.entries[&1].devtools_front.is_some());
        // Two at once settle two; more windows than requests settle what there is.
        fronted(&mut s, &[3, 1], 5);
        assert!(s.entries[&3].devtools_front.is_none() && s.entries[&1].devtools_front.is_none());
    }
}
