//! YAML front matter extraction.

use crate::types::{ParseWarning, Status};
use serde_json::{Map, Value};

/// The interpreted front matter of a MADR document.
#[derive(Debug, Clone, Default)]
pub struct FrontMatter {
    pub status_raw: Option<String>,
    pub status: Option<Status>,
    pub date: Option<String>,
    pub decision_makers: Vec<String>,
    pub consulted: Vec<String>,
    pub informed: Vec<String>,
    /// Keys Adrian does not interpret, preserved so nothing is lost on export.
    pub extra: Map<String, Value>,
}

/// Split front matter from the body. Returns the front matter and the byte offset at
/// which the body starts.
pub fn parse(text: &str, warnings: &mut Vec<ParseWarning>) -> (FrontMatter, usize) {
    let Some(rest) = text.strip_prefix("---") else {
        return (FrontMatter::default(), 0);
    };
    // The delimiter must be followed by a newline, otherwise `---foo` is just text.
    let after = match rest.strip_prefix("\r\n") {
        Some(r) => (r, 5),
        None => match rest.strip_prefix('\n') {
            Some(r) => (r, 4),
            None => return (FrontMatter::default(), 0),
        },
    };
    let (body_after_open, open_len) = after;

    // Find the closing delimiter at the start of a line.
    let mut offset = 0usize;
    let mut close_at = None;
    for line in body_after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            close_at = Some(offset);
            break;
        }
        offset += line.len();
    }

    let Some(close_at) = close_at else {
        warnings.push(ParseWarning::InvalidFrontMatter {
            message: "front matter opened but never closed".to_string(),
        });
        return (FrontMatter::default(), 0);
    };

    let yaml = &body_after_open[..close_at];
    let body_start = open_len + close_at + 4; // `---` plus its newline

    match serde_yaml::from_str::<serde_yaml::Value>(yaml) {
        Ok(value) => {
            let front = interpret(&value, warnings);
            (front, body_start.min(text.len()))
        }
        Err(err) => {
            warnings.push(ParseWarning::InvalidFrontMatter {
                message: err.to_string(),
            });
            (FrontMatter::default(), body_start.min(text.len()))
        }
    }
}

fn interpret(value: &serde_yaml::Value, warnings: &mut Vec<ParseWarning>) -> FrontMatter {
    let mut front = FrontMatter::default();
    let serde_yaml::Value::Mapping(map) = value else {
        return front;
    };

    for (key, value) in map {
        let Some(key) = key.as_str() else { continue };
        let normalized = key.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "status" => {
                if let Some(raw) = scalar_to_string(value) {
                    if !raw.trim().is_empty() {
                        match Status::parse_loose(&raw) {
                            Some(status) => front.status = Some(status),
                            None => warnings.push(ParseWarning::UnknownStatus { raw: raw.clone() }),
                        }
                        front.status_raw = Some(raw);
                    }
                }
            }
            "date" => front.date = scalar_to_string(value).filter(|s| !s.trim().is_empty()),
            "decision-makers" | "decision_makers" | "deciders" => {
                front.decision_makers = to_string_list(value)
            }
            "consulted" => front.consulted = to_string_list(value),
            "informed" => front.informed = to_string_list(value),
            _ => {
                if let Some(json) = yaml_to_json(value) {
                    front.extra.insert(key.to_string(), json);
                }
            }
        }
    }

    front
}

/// Render a scalar as a string, or `None` for null/empty.
fn scalar_to_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::Null => None,
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        other => yaml_to_json(other).map(|v| match v {
            Value::String(s) => s,
            other => other.to_string(),
        }),
    }
}

/// Normalise the three shapes MADR authors use for list-valued keys: a YAML sequence,
/// a comma-separated string, or a single scalar.
fn to_string_list(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Null => Vec::new(),
        serde_yaml::Value::Sequence(seq) => seq
            .iter()
            .filter_map(scalar_to_string)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        other => match scalar_to_string(other) {
            Some(s) => s
                .split(',')
                .map(|part| part.trim().to_string())
                .filter(|part| !part.is_empty())
                .collect(),
            None => Vec::new(),
        },
    }
}

/// Convert a YAML value into JSON so it can be stored verbatim.
pub fn yaml_to_json(value: &serde_yaml::Value) -> Option<Value> {
    Some(match value {
        serde_yaml::Value::Null => Value::Null,
        serde_yaml::Value::Bool(b) => Value::Bool(*b),
        serde_yaml::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Number(i.into())
            } else if let Some(u) = n.as_u64() {
                Value::Number(u.into())
            } else {
                let f = n.as_f64()?;
                serde_json::Number::from_f64(f).map(Value::Number)?
            }
        }
        serde_yaml::Value::String(s) => Value::String(s.clone()),
        serde_yaml::Value::Sequence(seq) => {
            Value::Array(seq.iter().filter_map(yaml_to_json).collect())
        }
        serde_yaml::Value::Mapping(map) => {
            let mut out = Map::new();
            for (k, v) in map {
                let key = match k {
                    serde_yaml::Value::String(s) => s.clone(),
                    other => scalar_to_string(other).unwrap_or_default(),
                };
                if let Some(json) = yaml_to_json(v) {
                    out.insert(key, json);
                }
            }
            Value::Object(out)
        }
        serde_yaml::Value::Tagged(tagged) => yaml_to_json(&tagged.value)?,
    })
}
