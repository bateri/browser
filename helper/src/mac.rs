//! The NSApplication subclass Chromium's macOS embedding needs (`CefAppProtocol`).

use cef::application_mac::{CefAppProtocol, CrAppControlProtocol, CrAppProtocol};
use objc2::DefinedClass;
use objc2::runtime::NSObjectProtocol;
use objc2::{
    ClassType, MainThreadMarker, define_class, extern_methods, msg_send,
    rc::Retained,
    runtime::{AnyObject, Bool},
};
use objc2_app_kit::{
    NSApp, NSApplication, NSEvent, NSEventMask, NSEventModifierFlags, NSEventType, NSWindow,
};
use objc2_foundation::{NSDate, NSDefaultRunLoopMode, NSPoint};
use std::cell::{Cell, RefCell};

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

        /// The engine stamps each mouse and key event it is handed with the time of the
        /// application's current event (with none, the mouse ones get the engine's own clock and
        /// the keys no time at all). Off screen no system event stands behind bateri's input, and
        /// once the event queue has been run for a window of the helper's own (`pump_events`) the
        /// current event is whichever came last, minutes or hours old. A page that changes its
        /// address on a click (YouTube) then dated the click before the page itself, and
        /// Chromium's check on that took the page's process down. So while the helper hands input
        /// over, the current event is one of that moment.
        #[unsafe(method_id(currentEvent))]
        unsafe fn current_event(&self) -> Option<Retained<NSEvent>> {
            INPUT
                .with_borrow(|input| input.clone())
                .or_else(|| unsafe { msg_send![super(self), currentEvent] })
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

thread_local! {
    /// What `currentEvent` answers while the helper hands bateri's input to the engine
    /// (`input_now`). Not an instance variable: the application object is made by AppKit, so one
    /// that needs dropping is never initialized.
    static INPUT: RefCell<Option<Retained<NSEvent>>> = const { RefCell::new(None) };
}

/// The clock event times are read on: seconds since the Mac started, sleep not counted (what
/// `mach_absolute_time` counts, and what Chromium's own clock reads).
fn uptime() -> f64 {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `now` is a timespec to write into.
    unsafe { libc::clock_gettime(libc::CLOCK_UPTIME_RAW, &mut now) };
    now.tv_sec as f64 + now.tv_nsec as f64 / 1e9
}

/// Runs `send` (the helper handing one of bateri's mouse, wheel or key events to the engine) with
/// `[NSApp currentEvent]` an event of this moment: the engine dates the input by it.
pub fn input_now<R>(send: impl FnOnce() -> R) -> R {
    let now = NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
        NSEventType::ApplicationDefined,
        NSPoint::new(0.0, 0.0),
        NSEventModifierFlags::empty(),
        uptime(),
        0,
        None,
        0,
        0,
        0,
    );
    let before = INPUT.replace(now);
    let out = send();
    drop(INPUT.replace(before));
    out
}

thread_local! {
    /// Windows that are not the ones a developer-tools request is waiting for: the ones there were
    /// when it was made, and the ones already brought forward. They are held (retained), so a
    /// window's address is never taken for another's after it is gone.
    static CLAIMED: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
}

fn same(a: &Retained<NSWindow>, b: &Retained<NSWindow>) -> bool {
    std::ptr::eq(Retained::as_ptr(a), Retained::as_ptr(b))
}

/// Called before the developer tools are asked for: every window there is now is not theirs. A
/// closed window stays in the application's list for good (measured), and bringing it forward
/// would show the tools of a page that was put away.
pub fn note_existing_windows() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let windows: Vec<Retained<NSWindow>> = NSApp(mtm).windows().iter().collect();
    CLAIMED.with_borrow_mut(|claimed| {
        for window in windows {
            if !claimed.iter().any(|c| same(c, &window)) {
                claimed.push(window);
            }
        }
    });
}

/// Puts windows nobody has claimed (the ones a request for the developer tools made since
/// `note_existing_windows`) in front, and the helper in front of the other apps: it is a
/// background app (`LSUIElement`), so the tools' window would otherwise open behind whatever the
/// person is looking at. Returns how many are on screen now.
pub fn front_new_windows() -> usize {
    let Some(mtm) = MainThreadMarker::new() else {
        return 0;
    };
    let app = NSApp(mtm);
    let windows: Vec<Retained<NSWindow>> = app.windows().iter().collect();
    let mut shown = 0;
    let mut released = Vec::new();
    CLAIMED.with_borrow_mut(|claimed| {
        // What is gone from the list is let go of (after the borrow: AppKit may call back).
        let (kept, gone) = std::mem::take(claimed)
            .into_iter()
            .partition(|c| windows.iter().any(|w| same(w, c)));
        *claimed = kept;
        released = gone;
        for window in &windows {
            if claimed.iter().any(|c| same(c, window)) {
                continue;
            }
            if !window.isVisible() {
                window.makeKeyAndOrderFront(None);
            }
            if window.isVisible() {
                claimed.push(window.clone());
                shown += 1;
            }
        }
    });
    drop(released);
    if shown > 0 {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
    shown
}

/// Brings the windows that are on screen (the developer tools', the helper has no others) to the
/// front, and the helper in front of the other apps: the tools asked to look at another element
/// are already open, and their window may be behind whatever the person is looking at.
pub fn front_open_windows() {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApp(mtm);
    let mut any = false;
    for window in app.windows().iter().filter(|w| w.isVisible()) {
        window.makeKeyAndOrderFront(None);
        any = true;
    }
    if any {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
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
