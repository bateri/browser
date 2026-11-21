//! Whether a sideways scroll at a point would move anything (`Command::CanScroll`): what decides
//! whether a two-finger swipe is the page's (a carousel, a wide table) or goes back and forward.
//!
//! The engine does not say. A wheel event gets no answer (the protocol call that sends one returns
//! nothing, and the engine's own history swipe is in Chrome's window, which this engine does not
//! have), and the only thing it reports of scrolling is the offset of the main frame. So the page is
//! asked: the boxes from the one under the point up to the page, the way a wheel event chains.
//! What it cannot see is a script that takes the wheel (`preventDefault` in a `wheel` listener) and
//! a frame of another origin: the second is answered "it scrolls", so the page keeps the gesture and
//! never goes back by mistake.

use crate::state::{browser_of, emit, with_entry};
use browser_ipc::message::Event;
use cef::*;
use serde_json::{Value, json};

/// The question as a script: `cx`, `cy` are the point in CSS pixels of the view (not yet divided by
/// the pinch scale), `dir` the way the content would move (positive: to the right, so the offset
/// falls). `true` when something there takes it. A script that fails answers `true`: unknown means
/// the page keeps its gesture.
const SCRIPT: &str = r#"(function (cx, cy, dir) {
  const EPS = 1;
  const up = (n) => n.assignedSlot || n.parentElement || (n.getRootNode && n.getRootNode().host) || null;
  const css = (el) => el.ownerDocument.defaultView.getComputedStyle(el);
  const room = (el) => {
    const span = el.scrollWidth - el.clientWidth;
    const rtl = css(el).direction === 'rtl';
    const low = rtl ? -span : 0;
    const high = rtl ? 0 : span;
    return dir > 0 ? el.scrollLeft > low + EPS : el.scrollLeft < high - EPS;
  };
  const sideways = (o) => o.overflowX === 'auto' || o.overflowX === 'scroll' || o.overflowX === 'overlay';
  const kept = (o) => (o.overscrollBehaviorX || 'auto') !== 'auto';
  const rootTakes = (doc) => {
    const view = doc.defaultView;
    const root = doc.documentElement;
    const se = doc.scrollingElement || root;
    const of = (el) => view.getComputedStyle(el);
    const body = doc.body;
    // The body's overflow goes to the viewport when the root's is visible, and so does its overscroll.
    if (kept(of(root)) || (body && kept(of(body)))) return true;
    const box = of(root).overflowX === 'visible' && body ? body : root;
    const overflow = of(box).overflowX;
    if (overflow !== 'hidden' && overflow !== 'clip' && se.scrollWidth - se.clientWidth > EPS && room(se)) return true;
    const vv = view.visualViewport;
    if (vv && vv.scale > 1.01) {
      // Zoomed with the fingers: the visual viewport moves inside the layout one before history does.
      return dir > 0 ? vv.offsetLeft > EPS : vv.offsetLeft + vv.width < se.clientWidth - EPS;
    }
    return false;
  };
  const takes = (doc, x, y) => {
    let el = doc.elementFromPoint(x, y);
    for (let i = 0; el && el.shadowRoot && i < 8; i++) {
      const inner = el.shadowRoot.elementFromPoint(x, y);
      if (!inner || inner === el) break;
      el = inner;
    }
    if (el && /^i?frame$/i.test(el.tagName)) {
      const inner = el.contentDocument;
      if (!inner) return true;
      const r = el.getBoundingClientRect();
      if (takes(inner, x - r.left - el.clientLeft, y - r.top - el.clientTop)) return true;
    }
    const root = doc.documentElement;
    const propagated = css(root).overflowX === 'visible';
    for (let n = el; n; n = up(n)) {
      if (n === root || (n === doc.body && propagated)) continue;
      const o = css(n);
      if (sideways(o) && n.scrollWidth - n.clientWidth > EPS) {
        if (room(n)) return true;
        if (kept(o)) return true;
      }
    }
    return rootTakes(doc);
  };
  try {
    const vv = window.visualViewport;
    const scale = vv && vv.scale > 0 ? vv.scale : 1;
    const x = scale === 1 ? cx : vv.offsetLeft + cx / scale;
    const y = scale === 1 ? cy : vv.offsetTop + cy / scale;
    return takes(document, x, y);
  } catch (e) {
    return true;
  }
})"#;

/// The call that asks: `x`, `y` in the view's points, `dx` as the wheel's.
pub fn expression(x: f64, y: f64, dx: i32) -> String {
    // The wheel's sign is the engine's: positive moves the content to the right (measured by
    // `a_wheel_moves_the_page_by_the_sign_of_dx`).
    let dir = dx.signum();
    format!("{SCRIPT}({x}, {y}, {dir})")
}

/// What an answer says: whether something takes the scroll. Anything but a plain yes or no is a yes.
pub fn takes_it(success: bool, answer: &Value) -> bool {
    if !success {
        return true;
    }
    answer["result"]["value"].as_bool().unwrap_or(true)
}

/// Asks the page; the answer comes back as `Event::CanScroll`.
pub fn ask(id: u32, req: u32, x: i32, y: i32, dx: i32) {
    let Some(host) = browser_of(id).and_then(|b| b.host()) else {
        return;
    };
    let ratio = crate::rules::level_to_ratio(host.zoom_level());
    let script = expression(f64::from(x) / ratio, f64::from(y) / ratio, dx);
    let sent = crate::agent::call(
        id,
        "Runtime.evaluate",
        json!({ "expression": script, "returnByValue": true }),
        |message_id| {
            with_entry(id, |e| e.scroll_asks.insert(message_id, req));
        },
    );
    if !sent {
        answer(id, req, true);
    }
}

/// The answer to a protocol call; `false` for one that is not an answer to `ask`.
pub fn on_result(id: u32, message_id: i32, success: bool, bytes: &[u8]) -> bool {
    let Some(Some(req)) = with_entry(id, |e| e.scroll_asks.remove(&message_id)) else {
        return false;
    };
    let value: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    answer(id, req, takes_it(success, &value));
    true
}

fn answer(id: u32, req: u32, can: bool) {
    emit(Event::CanScroll {
        browser: id,
        req,
        can,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_question_carries_the_point_and_the_way() {
        let call = expression(12.5, 40.0, -30);
        assert!(call.ends_with("(12.5, 40, -1)"), "{call}");
        assert!(expression(1.0, 2.0, 7).ends_with("(1, 2, 1)"));
    }

    #[test]
    fn only_a_plain_no_lets_the_gesture_go() {
        let said = |value: Value| json!({ "result": { "type": "boolean", "value": value } });
        assert!(!takes_it(true, &said(json!(false))));
        assert!(takes_it(true, &said(json!(true))));
        // Unknown is the page's.
        assert!(takes_it(false, &Value::Null));
        assert!(takes_it(true, &json!({ "exceptionDetails": {} })));
        assert!(takes_it(true, &said(json!("no"))));
    }
}
