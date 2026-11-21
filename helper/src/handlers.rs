//! The engine's callbacks for one browser: painting, the page's own events, and everything the
//! engine would ask a person (menus, dialogs, files, downloads, permissions) turned into a question
//! to bateri.
//!
//! All of them run on the UI thread except the proxy credentials, which come from the IO thread
//! and so keep what they need in their own fields.

use crate::agent::Observer;
use crate::convert::{cursor_of, key_kind_of};
use crate::rules::{self, level_to_ratio};
use crate::state::{Creds, browser_of, emit, next_req, with, with_entry};
use crate::{agent, trace};
use browser_ipc::message::*;
use browser_ipc::{Frame as PixelFrame, KIND_POPUP, KIND_VIEW, Sent};
use cef::*;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn text(s: Option<&CefString>) -> String {
    s.map(|s| s.to_string()).unwrap_or_default()
}

fn userfree(s: CefStringUserfree) -> String {
    CefString::from(&s).to_string()
}

wrap_client! {
    pub struct HelperClient {
        id: u32,
        creds: Arc<Creds>,
    }

    impl Client {
        fn render_handler(&self) -> Option<RenderHandler> {
            Some(Render::new(self.id))
        }
        fn display_handler(&self) -> Option<DisplayHandler> {
            Some(Display::new(self.id))
        }
        fn load_handler(&self) -> Option<LoadHandler> {
            Some(Load::new(self.id))
        }
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(Life::new(self.id))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(Requests::new(self.id, self.creds.clone()))
        }
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(Keys::new(self.id))
        }
        fn context_menu_handler(&self) -> Option<ContextMenuHandler> {
            Some(Menus::new(self.id))
        }
        fn jsdialog_handler(&self) -> Option<JsdialogHandler> {
            Some(Dialogs::new(self.id))
        }
        fn dialog_handler(&self) -> Option<DialogHandler> {
            Some(Files::new(self.id))
        }
        fn download_handler(&self) -> Option<DownloadHandler> {
            Some(Downloads::new(self.id))
        }
        fn find_handler(&self) -> Option<FindHandler> {
            Some(Finds::new(self.id))
        }
        fn permission_handler(&self) -> Option<PermissionHandler> {
            Some(Permissions::new())
        }
    }
}

/// Sends the browser's state to bateri when it changed since the last time.
pub fn push_state(id: u32) {
    // The engine's answers (zoom, developer tools) come from calls, made before the state is
    // borrowed.
    let host = browser_of(id).and_then(|b| b.host());
    let zoom = host
        .as_ref()
        .map(|h| level_to_ratio(h.zoom_level()))
        .unwrap_or(1.0);
    let event = with_entry(id, |e| {
        let event = Event::State {
            browser: id,
            url: e.url.clone(),
            title: e.title.clone(),
            loading: e.loading,
            can_back: e.can_back,
            can_forward: e.can_forward,
            status: e.status.clone(),
            zoom,
            error: e.error.clone(),
            found: e.found,
        };
        if e.last_state.as_ref() == Some(&event) {
            return None;
        }
        e.last_state = Some(event.clone());
        Some(event)
    })
    .flatten();
    if let Some(event) = event {
        emit(event);
    }
}

wrap_render_handler! {
    struct Render {
        id: u32,
    }

    impl RenderHandler {
        fn view_rect(&self, _browser: Option<&mut Browser>, rect: Option<&mut Rect>) {
            let Some(rect) = rect else { return };
            let (w, h) = with_entry(self.id, |e| e.size).unwrap_or((800, 600));
            rect.x = 0;
            rect.y = 0;
            rect.width = w.max(1);
            rect.height = h.max(1);
        }

        fn screen_info(&self, _browser: Option<&mut Browser>, info: Option<&mut ScreenInfo>) -> ::std::os::raw::c_int {
            let Some(info) = info else { return 0 };
            let (w, h, scale) = with_entry(self.id, |e| (e.size.0, e.size.1, e.scale)).unwrap_or((800, 600, 1.0));
            info.device_scale_factor = scale;
            // The only screen the page knows is its own view, so a popup list is laid out inside it.
            info.rect = Rect { x: 0, y: 0, width: w.max(1), height: h.max(1) };
            info.available_rect = info.rect.clone();
            1
        }

        fn on_popup_show(&self, _browser: Option<&mut Browser>, show: ::std::os::raw::c_int) {
            if show == 0 {
                with_entry(self.id, |e| e.popup = None);
                emit(Event::Popup { browser: self.id, show: false, x: 0, y: 0, width: 0, height: 0 });
            }
        }

        fn on_popup_size(&self, _browser: Option<&mut Browser>, rect: Option<&Rect>) {
            let Some(r) = rect else { return };
            let scale = with_entry(self.id, |e| {
                e.popup = Some((r.x, r.y, r.width, r.height));
                e.scale
            })
            .unwrap_or(1.0);
            emit(Event::Popup {
                browser: self.id,
                show: true,
                x: (r.x as f32 * scale) as i32,
                y: (r.y as f32 * scale) as i32,
                width: (r.width as f32 * scale) as u32,
                height: (r.height as f32 * scale) as u32,
            });
        }

        fn on_accelerated_paint(
            &self,
            _browser: Option<&mut Browser>,
            type_: PaintElementType,
            _dirty: Option<&[Rect]>,
            info: Option<&AcceleratedPaintInfo>,
        ) {
            let Some(info) = info else { return };
            let popup = type_ != PaintElementType::default();
            // While a resize waits for the page, Chromium's capturer gives a frame of the new size
            // with the old page scaled into it and black around it (`content_rect` smaller than
            // `visible_rect`). Such a frame is not sent: the last whole one stays on the screen.
            let extra = &info.extra;
            let (c, v) = (&extra.content_rect, &extra.visible_rect);
            let whole = (c.x, c.y, c.width, c.height) == (v.x, v.y, v.width, v.height);
            if !popup && !whole {
                return;
            }
            let id = self.id;
            let scale_and_popup = with_entry(id, |e| (e.scale, e.popup));
            let Some((scale, popup_rect)) = scale_and_popup else { return };
            let (px, py) = popup_rect.map(|r| (r.0, r.1)).unwrap_or((0, 0));
            let outcome = with(|s| {
                s.seq += 1;
                let frame = PixelFrame {
                    browser: id,
                    kind: if popup { KIND_POPUP } else { KIND_VIEW },
                    width: extra.coded_size.width as u32,
                    height: extra.coded_size.height as u32,
                    x: (px as f32 * scale) as i32,
                    y: (py as f32 * scale) as i32,
                    seq: s.seq,
                    sent_us: 0,
                };
                // SAFETY: the engine hands the surface to this callback and keeps it alive until it
                // returns; the send makes its own right to it.
                s.frames.as_ref().map(|tx| unsafe { tx.send(frame, info.shared_texture_io_surface) })
            });
            match outcome {
                Some(Ok(Sent::Queued)) | None => {}
                // bateri is behind: nothing waits for it. The newest picture is asked for again
                // in a moment, so the last frame to be dropped is not the one that stays on screen.
                Some(Ok(Sent::Full)) => {
                    with_entry(id, |e| e.repaint_at = Some(Instant::now() + Duration::from_millis(30)));
                }
                Some(Err(e)) => trace!("frame send: {e}"),
            }
        }
    }
}

wrap_display_handler! {
    struct Display {
        id: u32,
    }

    impl DisplayHandler {
        fn on_title_change(&self, _browser: Option<&mut Browser>, title: Option<&CefString>) {
            with_entry(self.id, |e| e.title = text(title));
            push_state(self.id);
        }

        fn on_address_change(&self, _browser: Option<&mut Browser>, frame: Option<&mut Frame>, url: Option<&CefString>) {
            if frame.is_some_and(|f| f.is_main() != 0) {
                with_entry(self.id, |e| e.url = text(url));
                push_state(self.id);
            }
        }

        fn on_status_message(&self, _browser: Option<&mut Browser>, value: Option<&CefString>) {
            with_entry(self.id, |e| e.status = text(value));
            push_state(self.id);
        }

        fn on_cursor_change(
            &self,
            _browser: Option<&mut Browser>,
            _cursor: *mut u8,
            type_: CursorType,
            _info: Option<&CursorInfo>,
        ) -> ::std::os::raw::c_int {
            emit(Event::Cursor { browser: self.id, cursor: cursor_of(&type_) });
            1
        }
    }
}

wrap_load_handler! {
    struct Load {
        id: u32,
    }

    impl LoadHandler {
        fn on_loading_state_change(
            &self,
            _browser: Option<&mut Browser>,
            is_loading: ::std::os::raw::c_int,
            can_go_back: ::std::os::raw::c_int,
            can_go_forward: ::std::os::raw::c_int,
        ) {
            with_entry(self.id, |e| {
                if is_loading != 0 && !e.loading {
                    e.error = None;
                }
                e.loading = is_loading != 0;
                e.can_back = can_go_back != 0;
                e.can_forward = can_go_forward != 0;
            });
            push_state(self.id);
        }

        fn on_load_error(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            error_code: Errorcode,
            error_text: Option<&CefString>,
            failed_url: Option<&CefString>,
        ) {
            // A load the page itself cut short (a new navigation) is not an error to show.
            let code = error_code.get_raw();
            if code == -3 || !frame.is_some_and(|f| f.is_main() != 0) {
                return;
            }
            with_entry(self.id, |e| {
                e.error = Some(LoadError { url: text(failed_url), code, message: text(error_text) })
            });
            push_state(self.id);
        }
    }
}

wrap_life_span_handler! {
    struct Life {
        id: u32,
    }

    impl LifeSpanHandler {
        fn on_before_popup(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _popup_id: ::std::os::raw::c_int,
            target_url: Option<&CefString>,
            _target_frame_name: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
            _popup_features: Option<&PopupFeatures>,
            _window_info: Option<&mut WindowInfo>,
            _client: Option<&mut Option<Client>>,
            _settings: Option<&mut BrowserSettings>,
            _extra_info: Option<&mut Option<DictionaryValue>>,
            _no_javascript_access: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            // The engine opens no window: bateri decides what a new one means.
            ask_window(self.id, &text(target_url), target_disposition == WindowOpenDisposition::NEW_BACKGROUND_TAB);
            1
        }

        fn on_after_created(&self, browser: Option<&mut Browser>) {
            let Some(browser) = browser else { return };
            crate::commands::created(self.id, browser.clone());
        }

        fn do_close(&self, _browser: Option<&mut Browser>) -> ::std::os::raw::c_int {
            0
        }

        fn on_before_close(&self, _browser: Option<&mut Browser>) {
            crate::commands::closed(self.id);
        }
    }
}

/// A page asked for a new window: told to bateri when the address is one the helper loads.
fn ask_window(id: u32, url: &str, background: bool) {
    if rules::scheme_allowed(url, true) {
        emit(Event::Window {
            browser: id,
            url: url.into(),
            background,
        });
    } else {
        emit(Event::Refused {
            browser: id,
            url: url.into(),
            reason: "scheme".into(),
        });
    }
}

wrap_request_handler! {
    struct Requests {
        id: u32,
        creds: Arc<Creds>,
    }

    impl RequestHandler {
        fn on_before_browse(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut cef::Request>,
            _user_gesture: ::std::os::raw::c_int,
            _is_redirect: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            let Some(request) = request else { return 0 };
            let url = userfree(request.url());
            let main = frame.map(|f| f.is_main() != 0).unwrap_or(true);
            if rules::scheme_allowed(&url, main) {
                return 0;
            }
            emit(Event::Refused { browser: self.id, url, reason: "scheme".into() });
            1
        }

        fn on_open_urlfrom_tab(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            target_url: Option<&CefString>,
            target_disposition: WindowOpenDisposition,
            _user_gesture: ::std::os::raw::c_int,
        ) -> ::std::os::raw::c_int {
            ask_window(self.id, &text(target_url), target_disposition == WindowOpenDisposition::NEW_BACKGROUND_TAB);
            1
        }

        /// The proxy asked who this is. The answer is the profile's own and the same for every
        /// browser of it, so it never depends on which browser the engine happened to ask for.
        /// (This runs on the IO thread: nothing here may touch the UI thread's state.)
        fn auth_credentials(
            &self,
            _browser: Option<&mut Browser>,
            _origin_url: Option<&CefString>,
            is_proxy: ::std::os::raw::c_int,
            _host: Option<&CefString>,
            _port: ::std::os::raw::c_int,
            _realm: Option<&CefString>,
            _scheme: Option<&CefString>,
            callback: Option<&mut AuthCallback>,
        ) -> ::std::os::raw::c_int {
            // A site's own sign-in is the page's to show, not ours to answer.
            if is_proxy == 0 {
                return 0;
            }
            let Some(callback) = callback else { return 0 };
            if self.creds.allow() {
                callback.cont(Some(&self.creds.user.as_str().into()), Some(&self.creds.pass.as_str().into()));
            } else {
                callback.cancel();
            }
            1
        }

        fn on_render_process_terminated(
            &self,
            _browser: Option<&mut Browser>,
            status: TerminationStatus,
            _error_code: ::std::os::raw::c_int,
            error_string: Option<&CefString>,
        ) {
            let reason = format!("{:?} {}", sys::cef_termination_status_t::from(status), text(error_string));
            crate::commands::dismiss_all(self.id);
            agent::fail_all(self.id, "the page's process ended");
            emit(Event::Crashed { browser: self.id, reason: reason.trim().into() });
        }
    }
}

wrap_keyboard_handler! {
    struct Keys {
        id: u32,
    }

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            if let Some(e) = event {
                trace!("pre-key browser={} type={:?} vk={} native={} mods={:#x}", self.id, e.type_, e.windows_key_code, e.native_key_code, e.modifiers);
            }
            0
        }

        /// The page and the engine had their turn and did not use the key: it goes back to bateri.
        fn on_key_event(
            &self,
            _browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
        ) -> ::std::os::raw::c_int {
            let Some(e) = event else { return 0 };
            trace!("key browser={} type={:?} vk={} native={} mods={:#x}", self.id, e.type_, e.windows_key_code, e.native_key_code, e.modifiers);
            emit(Event::UnhandledKey {
                browser: self.id,
                kind: key_kind_of(&e.type_),
                key: crate::convert::key_data_of(e),
            });
            1
        }
    }
}

wrap_context_menu_handler! {
    struct Menus {
        id: u32,
    }

    impl ContextMenuHandler {
        fn run_context_menu(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(params), Some(model), Some(callback)) = (params, model, callback) else { return 0 };
            let mut items = Vec::new();
            for i in 0..model.count() {
                let kind = model.type_at(i);
                if kind == MenuItemType::SEPARATOR {
                    // Two separators in a row, or one at an end, say nothing.
                    if !items.is_empty() && items.last().is_some_and(|m: &MenuItem| !m.separator) {
                        items.push(MenuItem { command: 0, label: String::new(), enabled: false, checked: false, separator: true });
                    }
                } else if kind == MenuItemType::COMMAND || kind == MenuItemType::CHECK || kind == MenuItemType::RADIO {
                    items.push(MenuItem {
                        command: model.command_id_at(i),
                        label: rules::menu_label(&userfree(model.label_at(i))),
                        enabled: model.is_enabled_at(i) != 0,
                        checked: model.is_checked_at(i) != 0,
                        separator: false,
                    });
                }
            }
            if items.last().is_some_and(|m| m.separator) {
                items.pop();
            }
            if items.is_empty() {
                callback.cancel();
                return 1;
            }
            let non_empty = |s: String| (!s.is_empty()).then_some(s);
            let req = next_req();
            with(|s| s.pending.menus.insert(req, (self.id, callback.clone())));
            emit(Event::Menu {
                browser: self.id,
                req,
                x: params.xcoord(),
                y: params.ycoord(),
                items,
                link: non_empty(userfree(params.unfiltered_link_url())),
                source: non_empty(userfree(params.source_url())),
                selection: non_empty(userfree(params.selection_text())),
                editable: params.is_editable() != 0,
            });
            1
        }
    }
}

wrap_jsdialog_handler! {
    struct Dialogs {
        id: u32,
    }

    impl JsdialogHandler {
        fn on_jsdialog(
            &self,
            _browser: Option<&mut Browser>,
            origin_url: Option<&CefString>,
            dialog_type: JsdialogType,
            message_text: Option<&CefString>,
            default_prompt_text: Option<&CefString>,
            callback: Option<&mut JsdialogCallback>,
            _suppress_message: Option<&mut ::std::os::raw::c_int>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else { return 0 };
            let kind = if dialog_type == JsdialogType::CONFIRM {
                DialogKind::Confirm
            } else if dialog_type == JsdialogType::PROMPT {
                DialogKind::Prompt
            } else {
                DialogKind::Alert
            };
            ask_dialog(self.id, kind, text(origin_url), text(message_text), text(default_prompt_text), callback.clone());
            1
        }

        fn on_before_unload_dialog(
            &self,
            _browser: Option<&mut Browser>,
            message_text: Option<&CefString>,
            _is_reload: ::std::os::raw::c_int,
            callback: Option<&mut JsdialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else { return 0 };
            ask_dialog(self.id, DialogKind::BeforeUnload, String::new(), text(message_text), String::new(), callback.clone());
            1
        }

        /// The page moved on while a question was open: nobody is waiting for it any more.
        fn on_reset_dialog_state(&self, _browser: Option<&mut Browser>) {
            crate::commands::dismiss_dialogs(self.id);
        }
    }
}

fn ask_dialog(
    id: u32,
    kind: DialogKind,
    origin: String,
    message: String,
    default_text: String,
    callback: JsdialogCallback,
) {
    let req = next_req();
    with(|s| s.pending.dialogs.insert(req, (id, callback)));
    emit(Event::Dialog {
        browser: id,
        req,
        kind,
        origin,
        message,
        default_text,
    });
}

wrap_dialog_handler! {
    struct Files {
        id: u32,
    }

    impl DialogHandler {
        fn on_file_dialog(
            &self,
            _browser: Option<&mut Browser>,
            mode: FileDialogMode,
            title: Option<&CefString>,
            default_file_path: Option<&CefString>,
            accept_filters: Option<&mut CefStringList>,
            _accept_extensions: Option<&mut CefStringList>,
            _accept_descriptions: Option<&mut CefStringList>,
            callback: Option<&mut FileDialogCallback>,
        ) -> ::std::os::raw::c_int {
            let Some(callback) = callback else { return 0 };
            let mode = if mode == FileDialogMode::OPEN_MULTIPLE {
                FileMode::OpenMultiple
            } else if mode == FileDialogMode::OPEN_FOLDER {
                FileMode::Folder
            } else if mode == FileDialogMode::SAVE {
                FileMode::Save
            } else {
                FileMode::Open
            };
            let accept: Vec<String> = accept_filters.map(|l| l.clone().into_iter().collect()).unwrap_or_default();
            let req = next_req();
            with(|s| s.pending.files.insert(req, (self.id, callback.clone())));
            emit(Event::FileDialog {
                browser: self.id,
                req,
                mode,
                title: text(title),
                default_path: text(default_file_path),
                accept,
            });
            1
        }
    }
}

wrap_download_handler! {
    struct Downloads {
        id: u32,
    }

    impl DownloadHandler {
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            _url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            1
        }

        /// Nothing is written until bateri has said where: the callback waits for its answer, and
        /// dropping it unanswered ends the download.
        fn on_before_download(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            suggested_name: Option<&CefString>,
            callback: Option<&mut BeforeDownloadCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(item), Some(callback)) = (download_item, callback) else { return 0 };
            let req = next_req();
            with(|s| {
                s.pending.before_download.insert(req, (self.id, callback.clone()));
                s.pending.downloads.insert(item.id(), (req, None));
            });
            let size = item.total_bytes();
            emit(Event::Download {
                browser: self.id,
                req,
                url: userfree(item.original_url()),
                name: text(suggested_name),
                mime: userfree(item.mime_type()),
                size: (size > 0).then_some(size as u64),
            });
            1
        }

        fn on_download_updated(
            &self,
            _browser: Option<&mut Browser>,
            download_item: Option<&mut DownloadItem>,
            callback: Option<&mut DownloadItemCallback>,
        ) {
            let Some(item) = download_item else { return };
            let item_id = item.id();
            let done = item.is_complete() != 0;
            let canceled = item.is_canceled() != 0;
            let state = if done {
                DownloadState::Done
            } else if canceled {
                DownloadState::Canceled
            } else {
                DownloadState::Running
            };
            let total = item.total_bytes();
            let req = with(|s| {
                let entry = s.pending.downloads.get_mut(&item_id)?;
                entry.1 = callback.map(|c| c.clone());
                let req = entry.0;
                if state != DownloadState::Running {
                    s.pending.downloads.remove(&item_id);
                    s.pending.before_download.remove(&req);
                }
                Some(req)
            });
            if let Some(req) = req {
                emit(Event::DownloadProgress {
                    req,
                    state,
                    received: item.received_bytes().max(0) as u64,
                    total: (total > 0).then_some(total as u64),
                });
            }
        }
    }
}

wrap_find_handler! {
    struct Finds {
        id: u32,
    }

    impl FindHandler {
        fn on_find_result(
            &self,
            browser: Option<&mut Browser>,
            _identifier: ::std::os::raw::c_int,
            count: ::std::os::raw::c_int,
            _selection_rect: Option<&Rect>,
            active_match_ordinal: ::std::os::raw::c_int,
            _final_update: ::std::os::raw::c_int,
        ) {
            trace!("find result count={count} ordinal={active_match_ordinal}");
            // The engine counts a new search and selects nothing: the first match is asked for
            // once, so the search bar shows "1 of 3" at once, as a browser's does, and never
            // "0 of 3" in between.
            let again = with_entry(self.id, |e| {
                let waiting = count > 0 && active_match_ordinal == 0;
                match e.find_activate.as_mut() {
                    Some(start) if waiting => {
                        let ask = !start.asked;
                        start.asked = true;
                        Some(ask.then(|| (start.pattern.clone(), start.forward, start.case)))
                    }
                    _ => {
                        e.find_activate = None;
                        None
                    }
                }
            })
            .flatten();
            if let Some(ask) = again {
                if let Some((pattern, forward, case)) = ask
                    && let Some(host) = browser.and_then(|b| b.host())
                {
                    host.find(Some(&pattern.as_str().into()), forward as _, case as _, 1);
                }
                return;
            }
            with_entry(self.id, |e| {
                e.found = Some(Found { count: count.max(0) as u32, current: active_match_ordinal.max(0) as u32 })
            });
            push_state(self.id);
        }
    }
}

wrap_permission_handler! {
    struct Permissions;

    impl PermissionHandler {
        /// Nothing is granted: a page that asks for the camera, the microphone or the screen is
        /// told no, and nobody is asked.
        fn on_request_media_access_permission(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut MediaAccessCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                callback.cont(0);
            }
            1
        }

        fn on_show_permission_prompt(
            &self,
            _browser: Option<&mut Browser>,
            _prompt_id: u64,
            _requesting_origin: Option<&CefString>,
            _requested_permissions: u32,
            callback: Option<&mut PermissionPromptCallback>,
        ) -> ::std::os::raw::c_int {
            if let Some(callback) = callback {
                callback.cont(PermissionRequestResult::DENY);
            }
            1
        }
    }
}

/// Registers the developer-tools observer of a new browser.
pub fn observe(id: u32, browser: &Browser) -> Option<Registration> {
    let host = browser.host()?;
    let mut observer = Observer::new(id);
    host.add_dev_tools_message_observer(Some(&mut observer))
}
