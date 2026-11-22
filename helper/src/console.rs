//! The page's console as lines: what `AgentOp::Console` reads. The lines come from the protocol's
//! `Runtime` events, not from the engine's console callback: that one hands over only the text
//! the page's arguments print as (`console.log({a: 1})` arrives as `[object Object]`), where the
//! protocol carries a preview of each argument, and an uncaught error with its description.

use crate::state::{CONSOLE_LINES, with_entry};
use crate::sync::{self, Call};
use serde_json::{Value, json};

/// The most lines of an error's description that are kept (the rest is its stack).
const ERROR_LINES: usize = 6;
/// The most properties of an object a line shows.
const PROPERTIES: usize = 8;

/// A browser is up: the protocol is asked to tell of its console.
pub fn enable(id: u32) {
    sync::send_call(id, "Runtime.enable", json!({}), Call::Quiet);
}

/// An event of the protocol: a console line is kept when it is one.
pub fn on_event(id: u32, method: &str, params: &[u8]) {
    if !method.starts_with("Runtime.") {
        return;
    }
    // A page that is left takes its console with it, as the tools of the engine's own do.
    if method == "Runtime.executionContextsCleared" {
        with_entry(id, |e| e.console.clear());
        return;
    }
    let json: Value = serde_json::from_slice(params).unwrap_or(Value::Null);
    let line = match method {
        "Runtime.consoleAPICalled" => line_of_call(&json),
        "Runtime.exceptionThrown" => line_of_exception(&json),
        _ => None,
    };
    let Some(line) = line else { return };
    crate::trace!("console {id} {line}");
    with_entry(id, |e| {
        if e.console.len() == CONSOLE_LINES {
            e.console.pop_front();
        }
        e.console.push_back(line);
    });
}

/// `console.log(...)` and its kin as a line: `[log] text (address:line)`.
pub fn line_of_call(event: &Value) -> Option<String> {
    let level = match event["type"].as_str()? {
        "log" | "dir" | "dirxml" | "table" | "trace" | "count" | "timeEnd" => "log",
        "debug" => "debug",
        "info" => "info",
        "warning" => "warning",
        "error" | "assert" => "error",
        _ => return None,
    };
    let args: Vec<String> = event["args"]
        .as_array()
        .map(|a| a.iter().map(show).collect())
        .unwrap_or_default();
    let frame = &event["stackTrace"]["callFrames"][0];
    Some(format!("[{level}] {}{}", args.join(" "), place(frame)))
}

/// An uncaught error (or a promise nobody caught) as a line.
pub fn line_of_exception(event: &Value) -> Option<String> {
    let details = &event["exceptionDetails"];
    let text = details["text"].as_str().unwrap_or("Uncaught");
    let what = match details["exception"]["description"].as_str() {
        Some(d) => first_lines(d, ERROR_LINES),
        None => show(&details["exception"]),
    };
    let place = place(&json!({
        "url": details["url"],
        "lineNumber": details["lineNumber"],
    }));
    Some(format!("[error] {text} {what}{place}"))
}

fn place(frame: &Value) -> String {
    let url = frame["url"].as_str().unwrap_or("");
    if url.is_empty() {
        return String::new();
    }
    match frame["lineNumber"].as_i64() {
        Some(line) => format!(" ({url}:{})", line + 1),
        None => format!(" ({url})"),
    }
}

fn first_lines(text: &str, n: usize) -> String {
    text.lines().take(n).collect::<Vec<_>>().join("\n")
}

/// One argument the way the page's console would print it.
fn show(arg: &Value) -> String {
    match arg["type"].as_str() {
        Some("string") => arg["value"].as_str().unwrap_or("").to_string(),
        Some("undefined") => "undefined".into(),
        Some("number" | "boolean" | "bigint") => match arg["unserializableValue"].as_str() {
            Some(v) => v.to_string(),
            None => arg["value"].to_string(),
        },
        Some("object") if arg["subtype"].as_str() == Some("null") => "null".into(),
        Some("object") => object(arg),
        _ => arg["description"].as_str().unwrap_or("").to_string(),
    }
}

fn object(arg: &Value) -> String {
    let description = arg["description"].as_str().unwrap_or("Object");
    if arg["subtype"].as_str() == Some("error") {
        return first_lines(description, ERROR_LINES);
    }
    let Some(properties) = arg["preview"]["properties"].as_array() else {
        return description.to_string();
    };
    let overflow =
        arg["preview"]["overflow"].as_bool().unwrap_or(false) || properties.len() > PROPERTIES;
    let shown = properties.iter().take(PROPERTIES);
    if arg["subtype"].as_str() == Some("array") {
        let mut items: Vec<String> = shown.map(property_value).collect();
        if overflow {
            items.push("…".into());
        }
        return format!("{description} [{}]", items.join(", "));
    }
    let mut items: Vec<String> = shown
        .map(|p| {
            format!(
                "{}: {}",
                p["name"].as_str().unwrap_or("?"),
                property_value(p)
            )
        })
        .collect();
    if overflow {
        items.push("…".into());
    }
    let name = if description == "Object" {
        ""
    } else {
        description
    };
    format!(
        "{name}{}{{{}}}",
        if name.is_empty() { "" } else { " " },
        items.join(", ")
    )
}

fn property_value(property: &Value) -> String {
    let value = property["value"].as_str().unwrap_or("");
    match property["type"].as_str() {
        Some("string") => format!("{value:?}"),
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(args: Value) -> Value {
        json!({"type": "log", "args": args, "stackTrace": {"callFrames": [{"url": "http://a.test/app.js", "lineNumber": 11}]}})
    }

    #[test]
    fn primitives_print_as_the_page_would() {
        let line = line_of_call(&call(json!([
            {"type": "string", "value": "count"},
            {"type": "number", "value": 3},
            {"type": "boolean", "value": true},
            {"type": "undefined"},
            {"type": "object", "subtype": "null", "value": null},
        ])))
        .unwrap();
        assert_eq!(
            line,
            "[log] count 3 true undefined null (http://a.test/app.js:12)"
        );
    }

    #[test]
    fn an_object_and_an_array_show_their_preview() {
        let object = json!({"type": "object", "description": "Object", "preview": {"overflow": false, "properties": [
            {"name": "a", "type": "number", "value": "1"},
            {"name": "name", "type": "string", "value": "x"},
            {"name": "b", "type": "object", "value": "Array(2)"},
        ]}});
        let array = json!({"type": "object", "subtype": "array", "description": "Array(3)", "preview": {"overflow": true, "properties": [
            {"name": "0", "type": "number", "value": "1"},
            {"name": "1", "type": "number", "value": "2"},
        ]}});
        let line = line_of_call(&call(json!([object, array]))).unwrap();
        assert_eq!(
            line,
            "[log] {a: 1, name: \"x\", b: Array(2)} Array(3) [1, 2, …] (http://a.test/app.js:12)"
        );
    }

    #[test]
    fn a_warning_an_error_and_an_assertion_keep_their_level_and_a_clear_is_no_line() {
        let mut e = call(json!([{"type": "string", "value": "w"}]));
        e["type"] = json!("warning");
        assert!(line_of_call(&e).unwrap().starts_with("[warning] w"));
        e["type"] = json!("assert");
        assert!(line_of_call(&e).unwrap().starts_with("[error] w"));
        e["type"] = json!("clear");
        assert_eq!(line_of_call(&e), None);
    }

    #[test]
    fn an_uncaught_error_has_its_description_and_place() {
        let event = json!({"exceptionDetails": {
            "text": "Uncaught", "url": "http://a.test/", "lineNumber": 4,
            "exception": {"type": "object", "subtype": "error", "description": "Error: boom\n    at f (http://a.test/:5:9)"},
        }});
        assert_eq!(
            line_of_exception(&event).unwrap(),
            "[error] Uncaught Error: boom\n    at f (http://a.test/:5:9) (http://a.test/:5)"
        );
        let promise = json!({"exceptionDetails": {"text": "Uncaught (in promise)", "exception": {"type": "string", "value": "nope"}}});
        assert_eq!(
            line_of_exception(&promise).unwrap(),
            "[error] Uncaught (in promise) nope"
        );
    }
}
