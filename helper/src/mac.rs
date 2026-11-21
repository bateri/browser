//! The NSApplication subclass Chromium's macOS embedding needs (`CefAppProtocol`).

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::DefinedClass;
use objc2::runtime::NSObjectProtocol;
use objc2::{
    ClassType, MainThreadMarker, define_class, extern_methods, msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool},
};
use objc2_app_kit::{NSApp, NSApplication, NSEvent, NSEventMask, NSWindow};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode};
use std::cell::Cell;

#[derive(Default)]
pub struct Ivars {
    handling_send_event: Cell<Bool>,
}

define_class!(
    #[unsafe(super(NSApplication))]
    #[ivars = Ivars]
    pub struct HelperApplication;

    impl HelperApplication {
        #[unsafe(method(sendEvent:))]
        unsafe fn send_event(&self, event: &NSEvent) {
            let was = self.is_handling_send_event();
            if !was {
                self.set_handling_send_event(true);
            }
            let _: () = msg_send![super(self), sendEvent: event];
            if !was {
                self.set_handling_send_event(false);
            }
        }

        /// The helper has no menus: a quit request (from the system, say) is the main loop's to
        /// see, so profiles are flushed on the way out.
        #[unsafe(method(terminate:))]
        unsafe fn terminate(&self, _sender: &AnyObject) {
            crate::request_quit();
        }
    }

    unsafe impl CrAppControlProtocol for HelperApplication {
        #[unsafe(method(setHandlingSendEvent:))]
        unsafe fn _set_handling_send_event(&self, handling: Bool) {
            self.ivars().handling_send_event.set(handling);
        }
    }

    unsafe impl CrAppProtocol for HelperApplication {
        #[unsafe(method(isHandlingSendEvent))]
        unsafe fn _is_handling_send_event(&self) -> Bool {
            self.ivars().handling_send_event.get()
        }
    }

    unsafe impl CefAppProtocol for HelperApplication {}
);

impl HelperApplication {
    extern_methods! {
        #[unsafe(method(sharedApplication))]
        fn shared_application() -> Retained<Self>;

        #[unsafe(method(setHandlingSendEvent:))]
        fn set_handling_send_event(&self, handling: bool);

        #[unsafe(method(isHandlingSendEvent))]
        fn is_handling_send_event(&self) -> bool;
    }
}

pub fn setup_application() {
    let _ = HelperApplication::shared_application();
    assert!(
        NSApp(MainThreadMarker::new().expect("main thread"))
            .isKindOfClass(HelperApplication::class())
    );
}

/// Puts the helper's windows in front, and the helper in front of the other apps: it is a
/// background app (`LSUIElement`), so the developer tools' window would otherwise open behind
/// whatever the person is looking at. Returns whether a window is up now.
pub fn show_windows() -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    let app = NSApp(mtm);
    let windows: Vec<Retained<NSWindow>> = app.windows().iter().collect();
    let mut shown = false;
    for window in &windows {
        if !window.isVisible() {
            window.makeKeyAndOrderFront(None);
        }
        shown |= window.isVisible();
    }
    if shown {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    shown
}

/// Whether a window of the helper's own (the developer tools') is on screen. A closed window
/// stays in the application's list (measured: it was still there seconds later), so the list
/// alone says nothing.
pub fn has_windows() -> bool {
    MainThreadMarker::new().is_some_and(|mtm| NSApp(mtm).windows().iter().any(|w| w.isVisible()))
}

/// Lets AppKit take its turn. The off-screen pages need none of it, but a window does: it is
/// ordered in, drawn, moved and typed into through the application's event queue, which nothing
/// here runs (the helper's own loop waits on bateri's commands, not on `[NSApp run]`). Called
/// every turn of the main loop while a window exists, so it must stay cheap.
pub fn pump_events() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApp(mtm);
    if app.windows().count() == 0 {
        return;
    }
    let past = NSDate::distantPast();
    loop {
        let event = unsafe {
            app.nextEventMatchingMask_untilDate_inMode_dequeue(
                NSEventMask::Any,
                Some(&past),
                NSDefaultRunLoopMode,
                true,
            )
        };
        match event {
            Some(event) => app.sendEvent(&event),
            None => break,
        }
    }
}
