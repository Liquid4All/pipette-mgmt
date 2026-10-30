//! A TOML document as the JSON data its schema describes.

use serde_json::Value;

/// `text` as JSON, each datetime as its text, which a schema calls a string.
pub fn toml_document(text: &str) -> Result<Value, toml::de::Error> {
    Ok(json(toml::Value::Table(text.parse()?)))
}

fn json(value: toml::Value) -> Value {
    match value {
        toml::Value::String(text) => text.into(),
        toml::Value::Integer(number) => number.into(),
        toml::Value::Float(number) => number.into(),
        toml::Value::Boolean(flag) => flag.into(),
        toml::Value::Datetime(at) => at.to_string().into(),
        toml::Value::Array(items) => items.into_iter().map(json).collect(),
        toml::Value::Table(table) => table
            .into_iter()
            .map(|(key, value)| (key, json(value)))
            .collect(),
    }
}
