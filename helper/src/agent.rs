//! What an agent asks of a page (its text, an outline, a click, typing, a picture), done through
//! the engine's DevTools protocol. A job is a short chain of protocol calls; each answer comes
//! back to `on_result`, which runs the next call or finishes the job.

use crate::state::{browser_of, emit, with_entry};
use browser_ipc::message::{AgentOp, AgentOutput, Event, Target};
use cef::*;
use serde_json::{Value, json};
use std::collections::VecDeque;

/// The longest text a job returns.
pub const MAX_TEXT: usize = 1 << 20;

pub struct Job {
    req: u32,
    kind: Kind,
    queue: VecDeque<(&'static str, Value)>,
}

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Snapshot,
    Shot,
    /// The call in flight asks where a node is; its answer becomes a click.
    Quads,
    /// Calls whose answers only matter by succeeding.
    Plain,
}

/// A flat JSON object as the engine's dictionary type.
fn to_dict(params: &Value) -> DictionaryValue {
    let dict = dictionary_value_create().expect("dictionary");
    if let Value::Object(map) = params {
        for (key, value) in map {
            let key: CefString = key.as_str().into();
            match value {
                Value::String(s) => {
                    dict.set_string(Some(&key), Some(&s.as_str().into()));
                }
                Value::Bool(b) => {
                    dict.set_bool(Some(&key), *b as _);
                }
                Value::Number(n) if n.is_i64() && n.as_i64().is_some_and(|v| v.abs() < 1 << 30) => {
                    dict.set_int(Some(&key), n.as_i64().unwrap_or(0) as _);
                }
                Value::Number(n) => {
                    dict.set_double(Some(&key), n.as_f64().unwrap_or(0.0));
                }
                Value::Object(_) => {
                    let mut inner = to_dict(value);
                    dict.set_dictionary(Some(&key), Some(&mut inner));
                }
                _ => {}
            }
        }
    }
    dict
}

pub fn start(id: u32, req: u32, op: AgentOp) {
    let (kind, queue): (Kind, Vec<(&'static str, Value)>) = match op {
        AgentOp::Text => (
            Kind::Text,
            vec![(
                "Runtime.evaluate",
                json!({"expression": "document.body ? document.body.innerText : ''", "returnByValue": true}),
            )],
        ),
        AgentOp::Snapshot => (
            Kind::Snapshot,
            vec![("Accessibility.getFullAXTree", json!({}))],
        ),
        AgentOp::Shot => (
            Kind::Shot,
            vec![("Page.captureScreenshot", json!({"format": "png"}))],
        ),
        AgentOp::Click { target } => click_plan(target),
        AgentOp::Type {
            text,
            target,
            enter,
        } => {
            let (mut kind, mut queue) = match target {
                Some(Target::Node { id }) => (
                    Kind::Plain,
                    vec![("DOM.focus", json!({"backendNodeId": id}))],
                ),
                Some(point @ Target::Point { .. }) => click_plan(point),
                None => (Kind::Plain, vec![]),
            };
            // After a click on a node the typing follows the click's own calls: they are added
            // when the quads answer arrives, so the text waits in the job's tail.
            let mut tail = vec![("Input.insertText", json!({"text": text}))];
            if enter {
                tail.push((
                    "Input.dispatchKeyEvent",
                    json!({"type": "rawKeyDown", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r"}),
                ));
                tail.push((
                    "Input.dispatchKeyEvent",
                    json!({"type": "keyUp", "key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13}),
                ));
            }
            if matches!(kind, Kind::Quads) {
                // Not reached: `Type` never targets a node by clicking it.
                kind = Kind::Plain;
            }
            queue.extend(tail);
            (kind, queue)
        }
    };
    let job = Job {
        req,
        kind,
        queue: queue.into(),
    };
    step(id, job);
}

fn click_plan(target: Target) -> (Kind, Vec<(&'static str, Value)>) {
    match target {
        Target::Point { x, y } => (Kind::Plain, mouse_calls(x, y)),
        Target::Node { id } => (
            Kind::Quads,
            vec![
                ("DOM.scrollIntoViewIfNeeded", json!({"backendNodeId": id})),
                ("DOM.getContentQuads", json!({"backendNodeId": id})),
            ],
        ),
    }
}

fn mouse_calls(x: f64, y: f64) -> Vec<(&'static str, Value)> {
    vec![
        (
            "Input.dispatchMouseEvent",
            json!({"type": "mouseMoved", "x": x, "y": y}),
        ),
        (
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x, "y": y, "button": "left", "clickCount": 1}),
        ),
        (
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x, "y": y, "button": "left", "clickCount": 1}),
        ),
    ]
}

fn finish(id: u32, req: u32, output: AgentOutput) {
    emit(Event::AgentResult {
        browser: id,
        req,
        output,
    });
}

/// Runs the job's next call, or finishes the job when none is left.
fn step(id: u32, mut job: Job) {
    let Some((method, params)) = job.queue.pop_front() else {
        finish(id, job.req, AgentOutput::Done);
        return;
    };
    let req = job.req;
    let Some(host) = browser_of(id).and_then(|b| b.host()) else {
        finish(id, req, failed("the browser is gone"));
        return;
    };
    let Some(message_id) = with_entry(id, |e| {
        e.next_cdp += 1;
        let mid = e.next_cdp;
        e.jobs.insert(mid, job);
        mid
    }) else {
        finish(id, req, failed("the browser is gone"));
        return;
    };
    crate::trace!("cdp {id}#{message_id} {method} {params}");
    let mut dict = to_dict(&params);
    let sent = host.execute_dev_tools_method(message_id, Some(&method.into()), Some(&mut dict));
    if sent == 0 {
        with_entry(id, |e| e.jobs.remove(&message_id));
        finish(id, req, failed(&format!("{method} was not sent")));
    }
}

fn failed(message: &str) -> AgentOutput {
    AgentOutput::Failed {
        message: message.into(),
    }
}

/// The answer to a protocol call.
pub fn on_result(id: u32, message_id: i32, success: bool, bytes: &[u8]) {
    let Some(Some(mut job)) = with_entry(id, |e| e.jobs.remove(&message_id)) else {
        return;
    };
    let json: Value = serde_json::from_slice(bytes).unwrap_or(Value::Null);
    crate::trace!(
        "cdp {id}#{message_id} -> ok={success} {} bytes",
        bytes.len()
    );
    if !success {
        let why = json["message"]
            .as_str()
            .unwrap_or("the protocol call failed");
        finish(id, job.req, failed(why));
        return;
    }
    match job.kind {
        Kind::Text => {
            if let Some(why) = json["exceptionDetails"]["text"].as_str() {
                finish(id, job.req, failed(why));
                return;
            }
            let text = json["result"]["value"].as_str().unwrap_or("");
            let (text, truncated) = cap_text(text, MAX_TEXT);
            finish(id, job.req, AgentOutput::Text { text, truncated });
        }
        Kind::Snapshot => {
            let (text, truncated) = cap_text(&outline(&json), MAX_TEXT);
            finish(id, job.req, AgentOutput::Text { text, truncated });
        }
        Kind::Shot => match json["data"].as_str() {
            Some(data) => finish(id, job.req, AgentOutput::Png { data: data.into() }),
            None => finish(id, job.req, failed("the page gave no picture")),
        },
        Kind::Quads => {
            // The calls before the quads (scrolling) only had to succeed.
            if job.queue.is_empty() {
                match quad_center(&json) {
                    Some((x, y)) => {
                        job.kind = Kind::Plain;
                        job.queue.extend(mouse_calls(x, y));
                        step(id, job);
                    }
                    None => finish(id, job.req, failed("the node has no box on the page")),
                }
            } else {
                step(id, job);
            }
        }
        Kind::Plain => step(id, job),
    }
}

/// A job whose browser went away answers that it did.
pub fn fail_all(id: u32, why: &str) {
    let Some(jobs) = with_entry(id, |e| std::mem::take(&mut e.jobs)) else {
        return;
    };
    for (_, job) in jobs {
        finish(id, job.req, failed(why));
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn cap_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// The middle of the first box of a `DOM.getContentQuads` answer (a quad is eight numbers).
pub fn quad_center(json: &Value) -> Option<(f64, f64)> {
    let quad = json["quads"].as_array()?.first()?.as_array()?;
    if quad.len() != 8 {
        return None;
    }
    let n: Vec<f64> = quad.iter().filter_map(Value::as_f64).collect();
    if n.len() != 8 {
        return None;
    }
    Some((
        (n[0] + n[2] + n[4] + n[6]) / 4.0,
        (n[1] + n[3] + n[5] + n[7]) / 4.0,
    ))
}

/// The accessibility tree as lines `role "name" [ref]`, indented by depth. `ref` is the node's
/// backend id: what `Target::Node` takes. Ignored nodes and the ones that say nothing (a bare
/// generic container, the pieces of a line of text) are left out, their children kept.
pub fn outline(tree: &Value) -> String {
    let Some(nodes) = tree["nodes"].as_array() else {
        return String::new();
    };
    let by_id: std::collections::HashMap<&str, &Value> = nodes
        .iter()
        .filter_map(|n| n["nodeId"].as_str().map(|id| (id, n)))
        .collect();
    let mut out = String::new();
    let Some(root) = nodes.first() else {
        return out;
    };
    let mut stack: Vec<(&Value, usize)> = vec![(root, 0)];
    while let Some((node, depth)) = stack.pop() {
        let role = node["role"]["value"].as_str().unwrap_or("");
        let name = node["name"]["value"].as_str().unwrap_or("");
        let value = node["value"]["value"].as_str().unwrap_or("");
        let ignored = node["ignored"].as_bool().unwrap_or(false);
        let silent = matches!(
            role,
            "" | "none" | "generic" | "InlineTextBox" | "LineBreak"
        ) && name.is_empty();
        let hidden = ignored || silent || matches!(role, "InlineTextBox" | "LineBreak");
        let mut next_depth = depth;
        if !hidden {
            out.push_str(&"  ".repeat(depth));
            out.push_str(role);
            if !name.is_empty() {
                out.push_str(&format!(" {name:?}"));
            }
            if !value.is_empty() {
                out.push_str(&format!(" value={value:?}"));
            }
            if let Some(backend) = node["backendDOMNodeId"].as_i64() {
                out.push_str(&format!(" [{backend}]"));
            }
            out.push('\n');
            next_depth = depth + 1;
        }
        if let Some(children) = node["childIds"].as_array() {
            // Pushed backwards so the first child pops first.
            for child in children.iter().rev().filter_map(Value::as_str) {
                if let Some(child) = by_id.get(child) {
                    stack.push((child, next_depth));
                }
            }
        }
    }
    out
}

// The developer-tools observer: its answers come to `on_result`.
wrap_dev_tools_message_observer! {
    pub struct Observer {
        id: u32,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_method_result(
            &self,
            _browser: Option<&mut Browser>,
            message_id: ::std::os::raw::c_int,
            success: ::std::os::raw::c_int,
            result: Option<&[u8]>,
        ) {
            on_result(self.id, message_id, success != 0, result.unwrap_or(&[]));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_cut_at_a_character_boundary() {
        assert_eq!(cap_text("abc", 10), ("abc".to_string(), false));
        assert_eq!(cap_text("abcdef", 3), ("abc".to_string(), true));
        // 'ç' is two bytes: a cut inside it backs up.
        assert_eq!(cap_text("açb", 2), ("a".to_string(), true));
    }

    #[test]
    fn a_quad_is_clicked_in_the_middle() {
        let quads = json!({"quads": [[10, 20, 30, 20, 30, 40, 10, 40]]});
        assert_eq!(quad_center(&quads), Some((20.0, 30.0)));
        assert_eq!(quad_center(&json!({"quads": []})), None);
        assert_eq!(quad_center(&json!({"quads": [[1, 2, 3]]})), None);
    }

    #[test]
    fn the_outline_names_nodes_and_keeps_the_children_of_silent_ones() {
        let tree = json!({"nodes": [
            {"nodeId": "1", "role": {"value": "RootWebArea"}, "name": {"value": "Title"}, "childIds": ["2"], "backendDOMNodeId": 1},
            {"nodeId": "2", "role": {"value": "generic"}, "name": {"value": ""}, "childIds": ["3", "4", "5"]},
            {"nodeId": "3", "role": {"value": "button"}, "name": {"value": "Go"}, "childIds": ["6"], "backendDOMNodeId": 12},
            {"nodeId": "4", "role": {"value": "textbox"}, "name": {"value": "Name"}, "value": {"value": "ada"}, "backendDOMNodeId": 13},
            {"nodeId": "5", "ignored": true, "role": {"value": "paragraph"}, "name": {"value": "hidden"}},
            {"nodeId": "6", "role": {"value": "InlineTextBox"}, "name": {"value": "Go"}}
        ]});
        assert_eq!(
            outline(&tree),
            "RootWebArea \"Title\" [1]\n  button \"Go\" [12]\n  textbox \"Name\" value=\"ada\" [13]\n"
        );
        assert_eq!(outline(&json!({})), "");
    }
}
