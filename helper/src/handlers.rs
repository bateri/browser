//! The engine's callbacks for one browser: painting, the page's own events, and everything the
//! engine would ask a person (menus, dialogs, files, downloads, permissions) turned into a question
//! to bateri.
//!
//! All of them run on the UI thread except the proxy credentials, which come from the IO thread
//! and so keep what they need in their own fields.

use crate::agent::Observer;
use crate::convert::{NET_ERROR_HEADER, cursor_of, key_kind_of, net_error_code, net_error_name};
use crate::rules::{self, level_to_ratio};
use crate::state::{
    Creds, Download, OpenMenu, browser_of, emit, next_req, take_where, with, with_entry,
};
use crate::{agent, clear, state, trace};
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
                let url = text(url);
                clear::note_address(self.id, &url);
                with_entry(self.id, |e| e.url = url);
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
            if is_loading == 0 {
                crate::sync::page_loaded(self.id);
            }
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
            let url = text(failed_url);
            let host = clear::host_of_origin(&url).map(str::to_ascii_lowercase);
            with_entry(self.id, |e| {
                // A certificate error is one the person can go on from only if the helper kept the
                // certificate it was refused for (`Command::Proceed`).
                let certificate = host
                    .and_then(|h| e.rejected.get(&h))
                    .and_then(|r| clear::certificate_reason(code, r.self_signed))
                    .map(str::to_string);
                e.error = Some(LoadError { url, code, message: text(error_text), certificate })
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

// The developer tools' own window. It has a client of its own, never the page's `HelperClient`
// (whose life-span handler would stand for the page itself): what the tools' page does is held to
// the tools' own page, and a window it asks for is bateri's to decide, like a page's.
wrap_client! {
    pub struct ToolsClient {
        id: u32,
    }

    impl Client {
        fn life_span_handler(&self) -> Option<LifeSpanHandler> {
            Some(ToolsLife::new(self.id))
        }
        fn request_handler(&self) -> Option<RequestHandler> {
            Some(ToolsRequests::new(self.id))
        }
    }
}

wrap_life_span_handler! {
    struct ToolsLife {
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
            // The engine opens no window: the page the tools were opened for asks bateri.
            ask_window(self.id, &text(target_url), target_disposition == WindowOpenDisposition::NEW_BACKGROUND_TAB);
            1
        }

        fn on_after_created(&self, _browser: Option<&mut Browser>) {
            trace!("the developer tools of browser {} were created", self.id);
        }
    }
}

wrap_request_handler! {
    struct ToolsRequests {
        id: u32,
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
            trace!("developer tools of browser {}: navigation to {url} (main frame {main})", self.id);
            if rules::devtools_frame_allowed(&url, main) {
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
    }
}

/// A page asked for a new window: told to bateri when the address is one the helper loads.
fn ask_window(id: u32, url: &str, background: bool) {
    emit(rules::window_event(id, url, background));
}

// Sets the error of a page the proxy answered for a server it could not reach, on the UI thread
// (the answer is seen on the IO thread).
wrap_task! {
    struct NetErrorSet {
        id: u32,
        url: String,
        code: i32,
    }

    impl Task {
        fn execute(&self) {
            with_entry(self.id, |e| {
                e.error = Some(LoadError {
                    url: self.url.clone(),
                    code: self.code,
                    message: net_error_name(self.code),
                    certificate: None,
                })
            });
            push_state(self.id);
        }
    }
}

// For the page's own document: when bateri's proxy could not reach the server it answers itself (a
// 502 whose body says why) and names the Chromium error in a header, and the page has then failed
// to load as far as bateri is concerned. The engine draws the proxy's body; the sub-resources of a
// page (pictures, requests) have no error to report.
wrap_resource_request_handler! {
    struct Documents {
        id: u32,
    }

    impl ResourceRequestHandler {
        /// The default of an unanswered call is to cancel the request: this handler never
        /// interferes with the load.
        fn on_before_resource_load(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            _callback: Option<&mut Callback>,
        ) -> ReturnValue {
            ReturnValue::CONTINUE
        }

        /// IO thread: nothing here may touch the UI thread's state.
        fn on_resource_response(
            &self,
            _browser: Option<&mut Browser>,
            frame: Option<&mut Frame>,
            request: Option<&mut Request>,
            response: Option<&mut Response>,
        ) -> ::std::os::raw::c_int {
            let (Some(request), Some(response)) = (request, response) else { return 0 };
            if !frame.is_some_and(|f| f.is_main() != 0) || request.resource_type() != ResourceType::MAIN_FRAME {
                return 0;
            }
            // Only bateri's proxy answers with a 502 and this header: a page's own server (reached
            // through the proxy) sending the header on any other status is not a failed load.
            if response.status() != 502 {
                return 0;
            }
            let header = userfree(response.header_by_name(Some(&NET_ERROR_HEADER.into())));
            let Some(code) = net_error_code(&header) else { return 0 };
            let mut task = NetErrorSet::new(self.id, userfree(request.url()), code);
            post_task(ThreadId::UI, Some(&mut task));
            0
        }
    }
}

wrap_request_handler! {
    struct Requests {
        id: u32,
        creds: Arc<Creds>,
    }

    impl RequestHandler {
        /// Documents (not the sub-resources of a page) go through `Documents`.
        fn resource_request_handler(
            &self,
            _browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            _request: Option<&mut Request>,
            is_navigation: ::std::os::raw::c_int,
            _is_download: ::std::os::raw::c_int,
            _request_initiator: Option<&CefString>,
            _disable_default_handling: Option<&mut ::std::os::raw::c_int>,
        ) -> Option<ResourceRequestHandler> {
            (is_navigation != 0).then(|| Documents::new(self.id))
        }

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

        /// The engine did not trust a site's certificate. A host the person went on with (for this
        /// certificate, in this profile) goes on; any other fails the load, and what was refused is
        /// kept for `Command::Proceed` to take (the card says why, `on_load_error`).
        fn on_certificate_error(
            &self,
            _browser: Option<&mut Browser>,
            cert_error: Errorcode,
            request_url: Option<&CefString>,
            ssl_info: Option<&mut Sslinfo>,
            callback: Option<&mut Callback>,
        ) -> ::std::os::raw::c_int {
            let url = text(request_url);
            let Some(host) = clear::host_of_origin(&url).map(str::to_ascii_lowercase) else { return 0 };
            let certificate = ssl_info.and_then(|i| i.x509_certificate());
            let der = certificate
                .as_ref()
                .and_then(|c| c.derencoded())
                .map(|b| {
                    let mut bytes = vec![0u8; b.size()];
                    b.data(Some(&mut bytes), 0);
                    bytes
                })
                .unwrap_or_default();
            let name_of = |p: Option<X509CertPrincipal>| p.map(|p| userfree(p.display_name()));
            let self_signed = certificate.as_ref().is_some_and(|c| {
                let (subject, issuer) = (name_of(c.subject()), name_of(c.issuer()));
                subject.is_some() && subject == issuer
            });
            let code = cert_error.get_raw();
            trace!("certificate error {code} for {url}: {} bytes of certificate", der.len());
            let accepted = with(|s| {
                let context = s.entries.get(&self.id).map(|e| e.context.clone())?;
                Some(s.contexts.get(&context)?.accepted.contains(&(host.clone(), der.clone())))
            })
            .unwrap_or(false);
            if accepted {
                if let Some(callback) = callback {
                    callback.cont();
                }
                return 1;
            }
            with_entry(self.id, |e| e.rejected.insert(host, state::Rejected { der, self_signed }));
            0
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

/// A line between two groups of rows in the page's menu.
fn separator() -> MenuItem {
    MenuItem {
        command: 0,
        label: String::new(),
        enabled: false,
        checked: false,
        separator: true,
        shortcut: String::new(),
    }
}

/// The engine's "view page source" command number.
fn view_source_command() -> i32 {
    sys::cef_menu_id_t::MENU_ID_VIEW_SOURCE as i32
}

wrap_context_menu_handler! {
    struct Menus {
        id: u32,
    }

    impl ContextMenuHandler {
        fn run_context_menu(
            &self,
            browser: Option<&mut Browser>,
            _frame: Option<&mut Frame>,
            params: Option<&mut ContextMenuParams>,
            model: Option<&mut MenuModel>,
            callback: Option<&mut RunContextMenuCallback>,
        ) -> ::std::os::raw::c_int {
            let (Some(params), Some(model), Some(callback)) = (params, model, callback) else { return 0 };
            // "View source" opens a `view-source:` page, an address the helper never loads: it is
            // not offered, and `MenuResult` runs only what was.
            model.remove(view_source_command());
            let mut items = Vec::new();
            for i in 0..model.count() {
                let kind = model.type_at(i);
                if kind == MenuItemType::SEPARATOR {
                    // Two separators in a row, or one at an end, say nothing.
                    if !items.is_empty() && items.last().is_some_and(|m: &MenuItem| !m.separator) {
                        items.push(separator());
                    }
                } else if kind == MenuItemType::COMMAND || kind == MenuItemType::CHECK || kind == MenuItemType::RADIO {
                    items.push(MenuItem {
                        command: model.command_id_at(i),
                        label: rules::menu_label(&userfree(model.label_at(i))),
                        enabled: model.is_enabled_at(i) != 0,
                        checked: model.is_checked_at(i) != 0,
                        separator: false,
                        shortcut: String::new(),
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
            let windows = with_entry(self.id, |e| e.new_windows).unwrap_or_default();
            let own = |row: rules::OwnRow, enabled: bool| MenuItem {
                command: row.command(),
                label: row.label(windows).into(),
                enabled,
                checked: false,
                separator: false,
                shortcut: row.shortcut().into(),
            };
            // A link's rows, then an image's, at the start: what the page's own element can do comes
            // before the engine's rows. The helper runs them (`MenuResult`).
            let urls = rules::MenuUrls {
                open_link: userfree(params.link_url()),
                copy_link: userfree(params.unfiltered_link_url()),
                image: if params.media_type().get_raw() == ContextMenuMediaType::IMAGE.get_raw() {
                    userfree(params.source_url())
                } else {
                    String::new()
                },
            };
            let mut front = Vec::new();
            for group in urls.groups() {
                front.extend(group.into_iter().map(|row| own(row, true)));
                front.push(separator());
            }
            items.splice(0..0, front);
            // Zoom and Inspect at the end, as they are the browser's own and not the page's.
            let ratio = browser
                .and_then(|b| b.host())
                .map_or(1.0, |h| level_to_ratio(h.zoom_level()));
            items.push(separator());
            for row in [rules::OwnRow::ZoomIn, rules::OwnRow::ZoomOut, rules::OwnRow::ActualSize] {
                items.push(own(row, row.enabled(ratio)));
            }
            items.push(separator());
            items.push(own(rules::OwnRow::Inspect, true));
            let non_empty = |s: String| (!s.is_empty()).then_some(s);
            let req = next_req();
            let offered: Vec<i32> = items.iter().filter(|m| !m.separator && m.enabled).map(|m| m.command).collect();
            let at = (params.xcoord(), params.ycoord());
            let menu = OpenMenu { callback: callback.clone(), offered, at, urls };
            let stale = with(|s| s.pending.menus.insert(req, (self.id, menu)));
            drop(stale);
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
    let stale = with(|s| s.pending.dialogs.insert(req, (id, kind, callback)));
    drop(stale);
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
            let stale = with(|s| s.pending.files.insert(req, (self.id, callback.clone())));
            drop(stale);
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
        /// A download is taken from the addresses the helper loads (and a page's own `blob:`); one
        /// from any other is refused, and bateri is told so.
        fn can_download(
            &self,
            _browser: Option<&mut Browser>,
            url: Option<&CefString>,
            _request_method: Option<&CefString>,
        ) -> ::std::os::raw::c_int {
            let url = text(url);
            trace!("can_download browser={} url={url}", self.id);
            if rules::download_allowed(&url) {
                return 1;
            }
            emit(Event::Refused { browser: self.id, url, reason: "scheme".into() });
            0
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
            let stale = with(|s| {
                let before = s.pending.before_download.insert(req, (self.id, callback.clone()));
                let running = s.pending.downloads.insert((self.id, item.id()), Download { req, cancel: None });
                (before, running)
            });
            drop(stale);
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
            let key = (self.id, item.id());
            let interrupted = item.is_interrupted() != 0
                || item.interrupt_reason() != DownloadInterruptReason::NONE;
            let state = rules::download_state(
                item.is_complete() != 0,
                item.is_canceled() != 0,
                item.is_interrupted() != 0,
                item.interrupt_reason() != DownloadInterruptReason::NONE,
            );
            let total = item.total_bytes();
            // What leaves the table is dropped after the borrow: letting go of an engine handle can
            // call back into a handler.
            let taken = with(|s| {
                let download = s.pending.downloads.get_mut(&key)?;
                let req = download.req;
                let newest = (state == DownloadState::Running).then(|| callback.as_ref().map(|c| (**c).clone())).flatten();
                let older = std::mem::replace(&mut download.cancel, newest);
                if state == DownloadState::Running {
                    return Some((req, older, Vec::new(), Vec::new()));
                }
                let finished = take_where(&mut s.pending.downloads, |k, _| *k == key);
                let asked = s.pending.before_download.remove(&req);
                Some((req, older, finished, asked.into_iter().collect()))
            });
            let Some((req, older, finished, asked)) = taken else { return };
            drop(older);
            drop(finished);
            drop(asked);
            if interrupted && state == DownloadState::Canceled && let Some(callback) = callback {
                // Not resumed: it would be written to disk after bateri was told it is over.
                callback.cancel();
            }
            emit(Event::DownloadProgress {
                req,
                state,
                received: item.received_bytes().max(0) as u64,
                total: (total > 0).then_some(total as u64),
            });
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
