//! A trackpad pinch on the page, the way Chrome on a Mac handles it (`Command::Pinch`).
//!
//! The engine has no call for a pinch: a pinch made with its protocol (`Input.synthesizePinchGesture`)
//! reaches the page as `ctrl`+wheel events and then does nothing to the page in this engine, and
//! its touch pinch does nothing at all (measured). So the two halves of Chrome's pinch are made
//! here, each by what the engine does give:
//!
//! 1. **The page has it first.** Each update is a wheel event with `ctrlKey` set (`Input.dispatchMouseEvent`),
//!    which is what a map or a design canvas listens for. Chrome decides on the first one: if the page
//!    called `preventDefault` on it the pinch is the page's and nothing else happens. A listener put
//!    on the window for the length of the pinch says whether it did (the page cannot tell it is there:
//!    it holds a symbol-keyed property, and takes no part in the event).
//! 2. **Otherwise the page is zoomed visually**: the visual viewport scales (`Emulation.setPageScaleFactor`;
//!    the layout does not change, and where the page is clicked afterwards is where it was aimed), and
//!    the point between the fingers is kept where it is by moving the visual viewport
//!    (`DOM.scrollIntoViewIfNeeded` with the region that should show: the engine scrolls the visual
//!    viewport before the layout one, and a call with a region that is none of the page's moves
//!    nothing a page can hear but the viewport's own events).
//!
//! The work is a short chain of protocol calls per update, each waiting for the answer of the one
//! before. [`Machine`] is that chain with no engine in it: it says which call is next and is told
//! how the last one ended, so it is tested here; the glue at the end sends the calls.

use crate::state::with_entry;
use browser_ipc::message::PinchKind;
use serde_json::{Value, json};
use std::collections::VecDeque;

/// The visual viewport's scale is held to what Chrome's desktop allows.
pub const SCALE_MIN: f64 = 1.0;
pub const SCALE_MAX: f64 = 5.0;

/// The wheel event an update is to the page: `deltaY` is this much per unit of `ln(scale)`, negative
/// to zoom in (what the engine's own touchpad pinch makes of it, measured).
const WHEEL_PER_LN: f64 = 100.0;

/// The protocol's bit for `ctrl` in a mouse event's modifiers.
const CTRL: u32 = 2;

/// The window's marker: what the listener keeps, by a key no page can write by accident.
const MARK: &str = "Symbol.for('bateri.pinch')";

/// What the visual viewport is: its scale, where its corner is in the layout viewport and how big
/// the layout viewport is (all in CSS pixels).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Viewport {
    pub scale: f64,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

impl Viewport {
    /// How far the corner may go.
    fn room(&self, scale: f64) -> (f64, f64) {
        (
            (self.width - self.width / scale).max(0.0),
            (self.height - self.height / scale).max(0.0),
        )
    }

    /// Where the corner is once the scale is `scale` and the corner is held to what the page has.
    fn held(&self, scale: f64, x: f64, y: f64) -> (f64, f64) {
        let (mx, my) = self.room(scale);
        (x.clamp(0.0, mx), y.clamp(0.0, my))
    }

    /// The viewport after the scale is multiplied by `factor` around `at` (CSS pixels at scale 1 from
    /// the layout viewport's corner, which is where the fingers are): the point of the page under
    /// `at` stays under it as far as the page's size lets it.
    pub fn zoomed(&self, factor: f64, at: (f64, f64)) -> Viewport {
        let scale = (self.scale * factor).clamp(SCALE_MIN, SCALE_MAX);
        let under = (self.x + at.0 / self.scale, self.y + at.1 / self.scale);
        let (x, y) = self.held(scale, under.0 - at.0 / scale, under.1 - at.1 / scale);
        Viewport {
            scale,
            x,
            y,
            ..*self
        }
    }
}

/// What the person's fingers did, as the machine is told it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    /// `at` is where the fingers are, in the view's points; `view` the view's size.
    Begin {
        at: (f64, f64),
        view: (f64, f64),
    },
    Update {
        at: (f64, f64),
        factor: f64,
    },
    End,
}

/// A protocol call to make.
#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub method: &'static str,
    pub params: Value,
}

/// Whose the pinch is, said by the first update.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Undecided,
    /// The page prevented the first wheel event: nothing else is done.
    Page,
    Zoom,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    Install,
    Object,
    Wheel,
    Prevented,
    Scale,
    Scroll,
    Uninstall,
    /// The handle on the page's root element is let go.
    Release,
}

/// A pinch that has begun.
#[derive(Debug)]
struct Seq {
    view: (f64, f64),
    mode: Mode,
    viewport: Viewport,
    /// How far the layout viewport is scrolled: the visual viewport's page position is this plus
    /// its own.
    scroll: (f64, f64),
    /// The page's pixels per point of the view: 1 at 100 %, a half at 200 % as a page.
    per_point: f64,
    /// The page's root element, to scroll the region into view of.
    object: String,
    /// The update in flight.
    update: Option<((f64, f64), f64)>,
    /// What the viewport is to be after the scale is set.
    target: Option<Viewport>,
}

/// One browser's pinches, one call in flight at a time.
#[derive(Default, Debug)]
pub struct Machine {
    ops: VecDeque<Op>,
    stage: Option<Stage>,
    /// The number of the call in flight, once it was sent.
    mid: Option<i32>,
    seq: Option<Seq>,
}

fn evaluate(expression: String, by_value: bool) -> Call {
    Call {
        method: "Runtime.evaluate",
        params: json!({
            "expression": expression,
            "returnByValue": by_value,
            "silent": true,
            "objectGroup": "bateri-pinch",
        }),
    }
}

fn install() -> Call {
    evaluate(
        format!(
            "(() => {{ const k = {MARK}; if (!window[k]) {{ const s = {{ last: null }}; \
             s.h = e => {{ s.last = e; }}; window[k] = s; \
             window.addEventListener('wheel', s.h, {{ capture: true, passive: true }}); }} \
             const v = window.visualViewport; \
             return JSON.stringify({{ s: v.scale, x: v.offsetLeft, y: v.offsetTop, \
             w: v.width * v.scale, h: v.height * v.scale, iw: window.innerWidth, \
             sx: v.pageLeft - v.offsetLeft, sy: v.pageTop - v.offsetTop }}); }})()"
        ),
        true,
    )
}

fn root() -> Call {
    evaluate("document.documentElement".into(), false)
}

fn prevented() -> Call {
    evaluate(
        format!(
            "(() => {{ const s = window[{MARK}]; return !!(s && s.last && s.last.defaultPrevented); }})()"
        ),
        true,
    )
}

fn uninstall() -> Call {
    evaluate(
        format!(
            "(() => {{ const k = {MARK}; const s = window[k]; if (s) {{ \
             window.removeEventListener('wheel', s.h, {{ capture: true }}); delete window[k]; }} }})()"
        ),
        true,
    )
}

fn wheel(at: (f64, f64), factor: f64) -> Call {
    Call {
        method: "Input.dispatchMouseEvent",
        params: json!({
            "type": "mouseWheel",
            "x": at.0,
            "y": at.1,
            "deltaX": 0,
            "deltaY": -WHEEL_PER_LN * factor.ln(),
            "modifiers": CTRL,
            "pointerType": "mouse",
        }),
    }
}

/// The viewport and the layout scroll the page said at the start.
/// What the page said at the start: the viewport, the layout viewport's scroll and the page's pixels
/// per point of the view (`view_width` is the view's width in points).
fn read(value: &Value, view_width: f64) -> Option<(Viewport, (f64, f64), f64)> {
    let text = value["result"]["value"].as_str()?;
    let said: Value = serde_json::from_str(text).ok()?;
    let number = |key: &str| said[key].as_f64().filter(|n| n.is_finite());
    let viewport = Viewport {
        scale: number("s")?.clamp(SCALE_MIN, SCALE_MAX),
        x: number("x")?.max(0.0),
        y: number("y")?.max(0.0),
        width: number("w").filter(|w| *w > 0.0)?,
        height: number("h").filter(|h| *h > 0.0)?,
    };
    // From the layout viewport's own width (`innerWidth`, scrollbars in it): the visual viewport's
    // is without the scrollbars, and a ratio taken from it is off by their width.
    let per_point = number("iw").filter(|w| *w > 0.0)? / view_width.max(1.0);
    Some((
        viewport,
        (number("sx")?.max(0.0), number("sy")?.max(0.0)),
        per_point,
    ))
}

impl Machine {
    /// The fingers did `op`; the call to make now, if one is to be made.
    pub fn push(&mut self, op: Op) -> Option<Call> {
        match (op, self.ops.back_mut()) {
            // Updates that wait for the one in flight are one: the factors multiply, and the
            // fingers are where they are last.
            (
                Op::Update { at, factor },
                Some(Op::Update {
                    at: last,
                    factor: sum,
                }),
            ) => {
                *last = at;
                *sum *= factor;
            }
            (op, _) => self.ops.push_back(op),
        }
        self.pump()
    }

    /// Whether the call numbered `mid` is the one in flight.
    pub fn owns(&self, mid: i32) -> bool {
        self.mid == Some(mid)
    }

    /// The call in flight went out as `mid`.
    pub fn sent(&mut self, mid: i32) {
        self.mid = Some(mid);
    }

    /// The call in flight ended: with `ok`, and what the engine said. The next call, if there is one.
    pub fn answered(&mut self, ok: bool, value: &Value) -> Option<Call> {
        let stage = self.stage.take()?;
        self.mid = None;
        if !ok {
            return self.abort(stage);
        }
        match stage {
            Stage::Install => match (
                self.seq.as_ref().and_then(|seq| read(value, seq.view.0)),
                self.seq.as_mut(),
            ) {
                (Some((viewport, scroll, per_point)), Some(seq)) => {
                    seq.viewport = viewport;
                    seq.scroll = scroll;
                    seq.per_point = per_point;
                    self.stage = Some(Stage::Object);
                    Some(root())
                }
                _ => self.abort(stage),
            },
            Stage::Object => {
                let object = value["result"]["objectId"].as_str().map(str::to_owned);
                match (object, self.seq.as_mut()) {
                    (Some(object), Some(seq)) => {
                        seq.object = object;
                        self.pump()
                    }
                    _ => self.abort(stage),
                }
            }
            Stage::Wheel => match self.seq.as_ref()?.mode {
                Mode::Undecided => {
                    self.stage = Some(Stage::Prevented);
                    Some(prevented())
                }
                Mode::Zoom => self.zoom(),
                Mode::Page => self.pump(),
            },
            Stage::Prevented => {
                let took = value["result"]["value"].as_bool().unwrap_or(false);
                let seq = self.seq.as_mut()?;
                seq.mode = if took { Mode::Page } else { Mode::Zoom };
                if took { self.pump() } else { self.zoom() }
            }
            Stage::Scale => {
                let seq = self.seq.as_mut()?;
                let Some(target) = seq.target.take() else {
                    return self.pump();
                };
                // The engine kept the corner where it was (as far as the new scale lets it): a
                // call is needed only if the corner is to be somewhere else.
                let before = seq.viewport;
                seq.viewport = target;
                let (x, y) = before.held(target.scale, before.x, before.y);
                if (x - target.x).abs() < 0.5 && (y - target.y).abs() < 0.5 {
                    return self.pump();
                }
                // The region is in the layout's own units, which are the page's pixels times the page
                // zoom (measured: at 200 % a region in the page's pixels is taken for one that is
                // twice as far from the corner and half as big, and the viewport does not move).
                let zoom = 1.0 / seq.per_point;
                let rect = json!({
                    "x": (seq.scroll.0 + target.x) * zoom,
                    "y": (seq.scroll.1 + target.y) * zoom,
                    "width": target.width / target.scale * zoom,
                    "height": target.height / target.scale * zoom,
                });
                self.stage = Some(Stage::Scroll);
                Some(Call {
                    method: "DOM.scrollIntoViewIfNeeded",
                    params: json!({ "objectId": seq.object, "rect": rect }),
                })
            }
            Stage::Scroll => self.pump(),
            Stage::Uninstall => {
                let object = self.seq.take().map(|seq| seq.object).unwrap_or_default();
                if object.is_empty() {
                    return self.pump();
                }
                self.seq = Some(Seq::ended(object));
                self.stage = Some(Stage::Release);
                Some(Call {
                    method: "Runtime.releaseObject",
                    params: json!({ "objectId": self.seq.as_ref()?.object }),
                })
            }
            Stage::Release => {
                self.seq = None;
                self.pump()
            }
        }
    }

    /// A call failed (the page went away, a navigation made the page's root a stranger): the pinch
    /// is let go, with the window's listener taken off where it can be.
    fn abort(&mut self, stage: Stage) -> Option<Call> {
        let seq = self.seq.take();
        self.ops.retain(|op| matches!(op, Op::Begin { .. }));
        if let Some(seq) = seq
            && !matches!(stage, Stage::Uninstall | Stage::Release)
        {
            // The listener's removal is a call of its own (and the handle's release after it).
            self.seq = Some(Seq::ended(seq.object));
            self.stage = Some(Stage::Uninstall);
            return Some(uninstall());
        }
        self.pump()
    }

    /// The next op's first call, when none is in flight.
    fn pump(&mut self) -> Option<Call> {
        loop {
            if self.stage.is_some() {
                return None;
            }
            match self.ops.pop_front()? {
                Op::Begin { view, .. } => {
                    self.seq = Some(Seq::new(view));
                    self.stage = Some(Stage::Install);
                    return Some(install());
                }
                Op::Update { at, factor } => {
                    let Some(seq) = self.seq.as_mut() else {
                        continue;
                    };
                    if !factor.is_finite() || factor <= 0.0 {
                        continue;
                    }
                    seq.update = Some((at, factor));
                    self.stage = Some(Stage::Wheel);
                    // The protocol's points are the page's own pixels, which are not the view's
                    // points when the page is zoomed as a page.
                    let per_point = seq.per_point;
                    return Some(wheel((at.0 * per_point, at.1 * per_point), factor));
                }
                Op::End => {
                    if self.seq.is_none() {
                        continue;
                    }
                    self.stage = Some(Stage::Uninstall);
                    return Some(uninstall());
                }
            }
        }
    }

    /// The zoom of the update in flight: the scale, and then the corner.
    fn zoom(&mut self) -> Option<Call> {
        let seq = self.seq.as_mut()?;
        let Some((at, factor)) = seq.update.take() else {
            return self.pump();
        };
        // CSS pixels per point of the view at scale 1: the page may be zoomed as a page too.
        let per_point = seq.per_point;
        let target = seq
            .viewport
            .zoomed(factor, (at.0 * per_point, at.1 * per_point));
        if target == seq.viewport {
            return self.pump();
        }
        seq.target = Some(target);
        self.stage = Some(Stage::Scale);
        Some(Call {
            method: "Emulation.setPageScaleFactor",
            params: json!({ "pageScaleFactor": target.scale }),
        })
    }
}

impl Seq {
    fn new(view: (f64, f64)) -> Seq {
        Seq {
            view,
            mode: Mode::Undecided,
            viewport: Viewport {
                scale: 1.0,
                x: 0.0,
                y: 0.0,
                width: view.0.max(1.0),
                height: view.1.max(1.0),
            },
            scroll: (0.0, 0.0),
            per_point: 1.0,
            object: String::new(),
            update: None,
            target: None,
        }
    }

    /// A pinch that is only waiting for its clean-up to be answered.
    fn ended(object: String) -> Seq {
        Seq {
            object,
            ..Seq::new((1.0, 1.0))
        }
    }
}

// ---- the engine's side ----

/// The fingers did `kind` at `(x, y)`; `scale` is an update's change.
pub fn apply(id: u32, kind: PinchKind, x: i32, y: i32, scale: f64) {
    let at = (f64::from(x), f64::from(y));
    let op = match kind {
        PinchKind::Begin => {
            let Some(view) = with_entry(id, |e| (f64::from(e.size.0), f64::from(e.size.1))) else {
                return;
            };
            Op::Begin { at, view }
        }
        PinchKind::Update => Op::Update { at, factor: scale },
        PinchKind::End => Op::End,
    };
    let call = with_entry(id, |e| e.pinch.push(op)).flatten();
    send(id, call);
}

/// The answer to a call of a pinch; `false` for one that is not.
pub fn on_result(id: u32, message_id: i32, success: bool, bytes: &[u8]) -> bool {
    if !with_entry(id, |e| e.pinch.owns(message_id)).unwrap_or(false) {
        return false;
    }
    let value: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    let next = with_entry(id, |e| e.pinch.answered(success, &value)).flatten();
    send(id, next);
    true
}

/// Sends the call, and what a call that cannot be sent leads to.
fn send(id: u32, mut call: Option<Call>) {
    while let Some(next) = call.take() {
        let sent = crate::agent::call(id, next.method, next.params, |message_id| {
            with_entry(id, |e| e.pinch.sent(message_id));
        });
        if !sent {
            call = with_entry(id, |e| e.pinch.answered(false, &Value::Null)).flatten();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn string(text: &str) -> Value {
        json!({ "result": { "type": "string", "value": text } })
    }

    fn boolean(on: bool) -> Value {
        json!({ "result": { "type": "boolean", "value": on } })
    }

    fn viewport(scale: f64, x: f64, y: f64) -> Value {
        // A 400 x 300 layout viewport.
        string(&format!(
            r#"{{"s":{scale},"x":{x},"y":{y},"w":400,"h":300,"iw":400,"sx":0,"sy":0}}"#
        ))
    }

    fn object() -> Value {
        json!({ "result": { "type": "object", "objectId": "root.1" } })
    }

    /// The fingers begin on a 400 x 300 view and the page answers the two calls that start a pinch.
    fn begun(scale: f64, x: f64, y: f64) -> Machine {
        let mut machine = Machine::default();
        let call = machine
            .push(Op::Begin {
                at: (200.0, 150.0),
                view: (400.0, 300.0),
            })
            .expect("the first call");
        assert!(
            call.params["expression"]
                .as_str()
                .unwrap()
                .contains("addEventListener")
        );
        let call = machine
            .answered(true, &viewport(scale, x, y))
            .expect("the root");
        assert_eq!(call.params["expression"], "document.documentElement");
        assert_eq!(machine.answered(true, &object()), None);
        machine
    }

    #[test]
    fn the_point_between_the_fingers_stays_where_it_is() {
        let v = Viewport {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
            width: 400.0,
            height: 300.0,
        };
        // A pinch out around (300, 150): the page's point there is still there.
        let z = v.zoomed(2.0, (300.0, 150.0));
        assert_eq!((z.scale, z.x, z.y), (2.0, 150.0, 75.0));
        assert_eq!(z.x + 300.0 / z.scale, v.x + 300.0 / v.scale);
        assert_eq!(z.y + 150.0 / z.scale, v.y + 150.0 / v.scale);
        // Another from there, off to the side: the point there is the page's (150 + 300/2).
        let again = z.zoomed(1.5, (100.0, 50.0));
        let page = (z.x + 100.0 / z.scale, z.y + 50.0 / z.scale);
        assert!((again.x + 100.0 / again.scale - page.0).abs() < 1e-9);
        assert!((again.y + 50.0 / again.scale - page.1).abs() < 1e-9);
    }

    #[test]
    fn the_scale_and_the_corner_are_held_to_what_the_page_has() {
        let v = Viewport {
            scale: 4.0,
            x: 290.0,
            y: 220.0,
            width: 400.0,
            height: 300.0,
        };
        // No more than 5, and the corner no further than the viewport's edge.
        let z = v.zoomed(3.0, (399.0, 299.0));
        assert_eq!(z.scale, SCALE_MAX);
        assert!(z.x <= 400.0 - 400.0 / 5.0 && z.y <= 300.0 - 300.0 / 5.0);
        // A pinch in goes back to the whole page and no further.
        let out = v.zoomed(0.01, (10.0, 10.0));
        assert_eq!((out.scale, out.x, out.y), (SCALE_MIN, 0.0, 0.0));
    }

    #[test]
    fn a_page_that_takes_the_pinch_is_given_wheel_events_and_nothing_else() {
        let mut machine = begun(1.0, 0.0, 0.0);
        let wheel = machine
            .push(Op::Update {
                at: (300.0, 150.0),
                factor: 1.5,
            })
            .expect("the wheel event");
        assert_eq!(wheel.method, "Input.dispatchMouseEvent");
        assert_eq!(wheel.params["modifiers"], 2);
        assert_eq!(
            (wheel.params["x"].as_f64(), wheel.params["y"].as_f64()),
            (Some(300.0), Some(150.0))
        );
        let dy = wheel.params["deltaY"].as_f64().unwrap();
        assert!((dy + 100.0 * 1.5f64.ln()).abs() < 1e-9, "{dy}");
        // The page prevented it: asked once, and every later update is a wheel event and no more.
        let ask = machine.answered(true, &json!({})).expect("is it prevented");
        assert!(
            ask.params["expression"]
                .as_str()
                .unwrap()
                .contains("defaultPrevented")
        );
        assert_eq!(machine.answered(true, &boolean(true)), None);
        for _ in 0..3 {
            let call = machine
                .push(Op::Update {
                    at: (300.0, 150.0),
                    factor: 1.1,
                })
                .expect("a wheel event");
            assert_eq!(call.method, "Input.dispatchMouseEvent");
            assert_eq!(machine.answered(true, &json!({})), None);
        }
        let end = machine.push(Op::End).expect("the listener goes");
        assert!(
            end.params["expression"]
                .as_str()
                .unwrap()
                .contains("removeEventListener")
        );
        let release = machine
            .answered(true, &Value::Null)
            .expect("the handle goes");
        assert_eq!(release.method, "Runtime.releaseObject");
        assert_eq!(release.params["objectId"], "root.1");
        assert_eq!(machine.answered(true, &Value::Null), None);
        // A pinch that is over is none: a stray update does nothing.
        assert_eq!(
            machine.push(Op::Update {
                at: (1.0, 1.0),
                factor: 1.1
            }),
            None
        );
    }

    #[test]
    fn a_page_that_does_not_take_the_pinch_is_zoomed_around_the_fingers() {
        let mut machine = begun(1.0, 0.0, 0.0);
        machine
            .push(Op::Update {
                at: (300.0, 150.0),
                factor: 2.0,
            })
            .expect("the wheel event");
        machine.answered(true, &json!({})).expect("is it prevented");
        let scale = machine.answered(true, &boolean(false)).expect("the scale");
        assert_eq!(scale.method, "Emulation.setPageScaleFactor");
        assert_eq!(scale.params["pageScaleFactor"], 2.0);
        // The engine kept the corner at the page's own: it is to be at (150, 75).
        let scroll = machine.answered(true, &json!({})).expect("the corner");
        assert_eq!(scroll.method, "DOM.scrollIntoViewIfNeeded");
        assert_eq!(scroll.params["objectId"], "root.1");
        assert_eq!(
            scroll.params["rect"],
            json!({ "x": 150.0, "y": 75.0, "width": 200.0, "height": 150.0 })
        );
        assert_eq!(machine.answered(true, &json!({})), None);
        // The next update is already decided: the wheel event, the scale (the corner follows if it
        // has to) without asking again.
        let wheel = machine
            .push(Op::Update {
                at: (300.0, 150.0),
                factor: 1.5,
            })
            .expect("a wheel event");
        assert_eq!(wheel.method, "Input.dispatchMouseEvent");
        let scale = machine.answered(true, &json!({})).expect("the scale");
        assert_eq!(scale.params["pageScaleFactor"], 3.0);
    }

    #[test]
    fn the_wheel_event_is_where_the_fingers_are_in_the_pages_own_pixels() {
        // A page zoomed to 200 % as a page: its layout viewport is 200 x 150 for a 400 x 300 view.
        let mut machine = Machine::default();
        machine
            .push(Op::Begin {
                at: (200.0, 100.0),
                view: (400.0, 300.0),
            })
            .expect("the first call");
        machine
            .answered(
                true,
                &string(r#"{"s":1,"x":0,"y":0,"w":200,"h":150,"iw":200,"sx":0,"sy":0}"#),
            )
            .expect("the root");
        machine.answered(true, &object());
        let wheel = machine
            .push(Op::Update {
                at: (200.0, 100.0),
                factor: 1.1,
            })
            .expect("the wheel event");
        assert_eq!(
            (wheel.params["x"].as_f64(), wheel.params["y"].as_f64()),
            (Some(100.0), Some(50.0))
        );
    }

    #[test]
    fn the_region_to_show_is_in_the_layouts_units_when_the_page_is_zoomed() {
        // 200 % as a page: a 200 x 150 layout viewport for a 400 x 300 view. Pinched out to 2 around
        // the middle, the viewport shows 100 x 75 of the page's pixels from (50, 37.5): the region
        // the call names is in layout units, twice those.
        let mut machine = Machine::default();
        machine
            .push(Op::Begin {
                at: (200.0, 150.0),
                view: (400.0, 300.0),
            })
            .expect("the first call");
        machine
            .answered(
                true,
                &string(r#"{"s":1,"x":0,"y":0,"w":200,"h":150,"iw":200,"sx":0,"sy":0}"#),
            )
            .expect("the root");
        machine.answered(true, &object());
        machine
            .push(Op::Update {
                at: (200.0, 150.0),
                factor: 2.0,
            })
            .expect("the wheel event");
        machine.answered(true, &json!({})).expect("is it prevented");
        machine.answered(true, &boolean(false)).expect("the scale");
        let scroll = machine.answered(true, &json!({})).expect("the corner");
        assert_eq!(
            scroll.params["rect"],
            json!({ "x": 100.0, "y": 75.0, "width": 200.0, "height": 150.0 })
        );
    }

    #[test]
    fn classic_scrollbars_do_not_skew_the_pixels_per_point() {
        // The visual viewport is 385 wide for a 400 wide view (a classic scrollbar takes 15), the
        // layout viewport 400: the page is at 100 %, and a point of the view is a pixel of the page.
        let mut machine = Machine::default();
        machine
            .push(Op::Begin {
                at: (200.0, 150.0),
                view: (400.0, 300.0),
            })
            .expect("the first call");
        machine
            .answered(
                true,
                &string(r#"{"s":1,"x":0,"y":0,"w":385,"h":285,"iw":400,"sx":0,"sy":0}"#),
            )
            .expect("the root");
        machine.answered(true, &object());
        let wheel = machine
            .push(Op::Update {
                at: (200.0, 100.0),
                factor: 1.1,
            })
            .expect("the wheel event");
        assert_eq!(
            (wheel.params["x"].as_f64(), wheel.params["y"].as_f64()),
            (Some(200.0), Some(100.0))
        );
    }

    #[test]
    fn what_comes_while_a_call_is_in_flight_is_one_update() {
        let mut machine = begun(1.0, 0.0, 0.0);
        machine
            .push(Op::Update {
                at: (100.0, 100.0),
                factor: 1.2,
            })
            .expect("the wheel event");
        // Three more wait for the answer, and are one.
        for at in [110.0, 120.0, 130.0] {
            assert_eq!(
                machine.push(Op::Update {
                    at: (at, at),
                    factor: 1.1,
                }),
                None
            );
        }
        machine.answered(true, &json!({})).expect("is it prevented");
        // It is not prevented: the zoom of the first, then the one that waited.
        let scale = machine.answered(true, &boolean(false)).expect("the scale");
        assert!((scale.params["pageScaleFactor"].as_f64().unwrap() - 1.2).abs() < 1e-9);
        machine.answered(true, &json!({})).expect("the corner");
        let wheel = machine
            .answered(true, &json!({}))
            .expect("the merged update");
        assert_eq!(wheel.method, "Input.dispatchMouseEvent");
        assert_eq!(
            (wheel.params["x"].as_f64(), wheel.params["y"].as_f64()),
            (Some(130.0), Some(130.0))
        );
        let dy = wheel.params["deltaY"].as_f64().unwrap();
        assert!(
            (dy + 100.0 * (1.1f64 * 1.1 * 1.1).ln()).abs() < 1e-9,
            "{dy}"
        );
    }

    #[test]
    fn a_zoom_that_changes_nothing_makes_no_calls_for_the_scale() {
        // Already whole and pinched in: nothing to scale, only the wheel event.
        let mut machine = begun(1.0, 0.0, 0.0);
        machine
            .push(Op::Update {
                at: (10.0, 10.0),
                factor: 0.5,
            })
            .expect("the wheel event");
        machine.answered(true, &json!({})).expect("is it prevented");
        assert_eq!(machine.answered(true, &boolean(false)), None);
    }

    #[test]
    fn a_failed_call_lets_the_pinch_go_and_takes_the_listener_off() {
        let mut machine = begun(2.0, 40.0, 30.0);
        machine
            .push(Op::Update {
                at: (100.0, 100.0),
                factor: 1.2,
            })
            .expect("the wheel event");
        machine.answered(true, &json!({})).expect("is it prevented");
        machine.answered(true, &boolean(false)).expect("the scale");
        // The scroll fails (the page left): the listener goes, and the updates after are nobody's.
        let mut machine2 = machine;
        let scroll = machine2.answered(true, &json!({})).expect("the corner");
        assert_eq!(scroll.method, "DOM.scrollIntoViewIfNeeded");
        let off = machine2
            .answered(false, &Value::Null)
            .expect("the listener goes");
        assert!(
            off.params["expression"]
                .as_str()
                .unwrap()
                .contains("removeEventListener")
        );
        let release = machine2
            .answered(true, &Value::Null)
            .expect("the handle goes");
        assert_eq!(release.method, "Runtime.releaseObject");
        // Even a release that fails leaves nothing behind.
        assert_eq!(machine2.answered(false, &Value::Null), None);
        assert_eq!(
            machine2.push(Op::Update {
                at: (1.0, 1.0),
                factor: 1.1
            }),
            None
        );
        // A new pinch begins as any.
        assert!(
            machine2
                .push(Op::Begin {
                    at: (1.0, 1.0),
                    view: (400.0, 300.0)
                })
                .is_some()
        );
    }

    #[test]
    fn a_page_zoomed_already_is_zoomed_from_where_it_is() {
        // Scale 2 with the corner at (100, 100): the point under (200, 150) is (200, 175).
        let mut machine = begun(2.0, 100.0, 100.0);
        machine
            .push(Op::Update {
                at: (200.0, 150.0),
                factor: 2.0,
            })
            .expect("the wheel event");
        machine.answered(true, &json!({})).expect("is it prevented");
        let scale = machine.answered(true, &boolean(false)).expect("the scale");
        assert_eq!(scale.params["pageScaleFactor"], 4.0);
        let scroll = machine.answered(true, &json!({})).expect("the corner");
        // At scale 4 the point stays under (200, 150): the corner is at (200 - 50, 175 - 37.5).
        assert_eq!(
            scroll.params["rect"],
            json!({ "x": 150.0, "y": 137.5, "width": 100.0, "height": 75.0 })
        );
    }
}
