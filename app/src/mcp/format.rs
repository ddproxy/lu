//! Sanitising and size-capping helpers for tool results.

use lu_proto::{printable, truncate_chars};
use serde_json::{Map, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use super::protocol::ToolOutput;

const PART_BUDGET: usize = 10 * 1024;

pub(super) fn clean(text: &str) -> String {
    text.split('\n')
        .map(printable)
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn one_line(text: &str) -> String {
    truncate_chars(&clean(text).replace('\n', " "), 500)
}

fn clean_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(clean(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(clean_value).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (clean(&key), clean_value(value)))
                .collect(),
        ),
        other => other,
    }
}

/// The controller stores an ESC as `?`, so both `ESC[...m` and `?[...m` must go.
pub(super) fn strip_ansi(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while at < bytes.len() {
        let start = if bytes[at] == b'\x1b' {
            at + 1
        } else if bytes[at] == b'?' && at + 1 < bytes.len() && bytes[at + 1] == b'[' {
            at + 2
        } else {
            0
        };
        if start > 0 {
            at = start;
            if at < bytes.len() && bytes[at] == b'[' {
                at += 1;
            }
            while at < bytes.len() && !bytes[at].is_ascii_alphabetic() {
                at += 1;
            }
            if at < bytes.len() {
                at += 1;
            }
            continue;
        }
        let ch = text[at..]
            .chars()
            .next()
            .expect("the cursor stays on a character boundary");
        out.push(ch);
        at += ch.len_utf8();
    }
    clean(&out)
}

pub(super) fn first_line(text: &str) -> String {
    strip_ansi(text)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned()
}

pub(super) fn cap_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    format!("{}\n[cut: text truncated]", truncate_chars(text, max))
}

fn json_len(value: &Value) -> usize {
    serde_json::to_string(value).map_or(usize::MAX, |text| text.len())
}

fn cap_string(text: &str, budget: usize) -> (String, bool) {
    let value = Value::String(text.to_owned());
    if json_len(&value) <= budget {
        return (text.to_owned(), false);
    }
    let mut end = budget.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut shortened = text[..end].to_owned();
    while json_len(&Value::String(shortened.clone())) > budget.saturating_sub(64) {
        shortened.pop();
    }
    (shortened, true)
}

fn cap_value(value: Value, budget: usize) -> (Value, bool) {
    if json_len(&value) <= budget {
        return (value, false);
    }
    match value {
        Value::String(text) => {
            let (text, _) = cap_string(&text, budget.saturating_sub(16));
            (Value::String(format!("{text}...[cut]")), true)
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            let mut used = 2;
            let mut cut = false;
            for item in items {
                let (item, item_cut) = cap_value(item, budget.saturating_sub(used + 16));
                let size = json_len(&item) + 1;
                if used + size > budget {
                    cut = true;
                    break;
                }
                used += size;
                out.push(item);
                cut |= item_cut;
            }
            if cut {
                out.push(Value::String("[cut]".to_owned()));
            }
            (Value::Array(out), true)
        }
        Value::Object(map) => {
            let mut out = Map::new();
            let mut used = 2;
            let mut cut = false;
            for (key, item) in map {
                let (item, item_cut) = cap_value(item, budget.saturating_sub(used + 16));
                let size = json_len(&Value::String(key.clone())) + json_len(&item) + 2;
                if used + size > budget {
                    cut = true;
                    break;
                }
                used += size;
                out.insert(key, item);
                cut |= item_cut;
            }
            if cut {
                out.insert("[cut]".to_owned(), Value::Bool(true));
            }
            (Value::Object(out), true)
        }
        other => (other, true),
    }
}

pub(super) fn cap_result(mut output: ToolOutput) -> ToolOutput {
    let (text, cut_text) = cap_string(&output.text, PART_BUDGET);
    let (structured, cut_json) = cap_value(clean_value(output.structured), PART_BUDGET);
    output.text = if cut_text || cut_json {
        format!("{text}\n[cut: result shortened to fit the 32 KiB limit]")
    } else {
        text
    };
    output.structured = structured;
    output
}

pub(super) fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
