//! The JSON Schemas of the bodies the app (`ui/`) sends and reads, as one
//! bundle. The app's TypeScript is generated from [`SCHEMA_PATH`], and the HTTP
//! tests check bodies against [`bundle`], so a body change fails until both agree.

use schemars::generate::SchemaSettings;
use schemars::transform::RemoveRefSiblings;
use schemars::transform::Transform as _;
use schemars::{Schema, SchemaGenerator};
use serde_json::{Map, Value};

use pipette_core::canonical_json::sort_keys;
use pipette_plan_types::{app_requests, app_responses};

pub mod openapi;

/// Where the bundle is committed, relative to the workspace root.
pub const SCHEMA_PATH: &str = "ui/src/lib/api-schema.json";

/// Every schema the app reads, under `$defs` by type name.
pub fn bundle() -> anyhow::Result<Value> {
    bundle_with(|_| {})
}

/// [`bundle`] with `extra` responses beside the app's. Fails on a name two
/// types share (see [`merged`]), which the committed-bundle test surfaces.
#[expect(
    clippy::disallowed_methods,
    reason = "the bundle is a JSON Schema document the app types itself from, not a body"
)]
fn bundle_with(extra: impl FnOnce(&mut SchemaGenerator)) -> anyhow::Result<Value> {
    let mut generator = SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator();
    app_responses(&mut generator);
    extra(&mut generator);
    let shown = generator.take_definitions(true);

    // A request is what the route accepts, so its schema is the deserialize
    // side's: a field with a default is optional.
    let mut generator = SchemaSettings::draft2020_12()
        .for_deserialize()
        .into_generator();
    app_requests(&mut generator);
    let definitions = merged(shown, generator.take_definitions(true))?;

    let mut root = Map::new();
    root.insert(
        "$schema".into(),
        "https://json-schema.org/draft/2020-12/schema".into(),
    );
    root.insert("title".into(), "ApiSchema".into());
    let mut defs = Value::Object(definitions);
    strip_descriptions(&mut defs);
    ref_siblings_into_all_of(&mut defs);
    root.insert("$defs".into(), defs);
    // A workspace build turns on `serde_json/preserve_order` (through the AWS
    // SDK), so without the sort the committed file would differ by build.
    Ok(sort_keys(Value::Object(root)))
}

/// Both sides' definitions as one map. Refuses a suffixed name (`Foo2`) or one
/// the sides define differently: its `$ref` would resolve to another shape.
fn merged(
    mut responses: Map<String, Value>,
    requests: Map<String, Value>,
) -> anyhow::Result<Map<String, Value>> {
    unsuffixed(&responses)?;
    unsuffixed(&requests)?;
    for (name, schema) in requests {
        match responses.get(&name) {
            None => {
                responses.insert(name, schema);
            }
            Some(shown) => anyhow::ensure!(
                *shown == schema,
                "`{name}` names a request and a response with different schemas: \
                 rename one type, or give a type read both ways a request-side mirror"
            ),
        }
    }
    Ok(responses)
}

/// Refuses a definition schemars named `{base}{n}`, `n` from 2, because a
/// second type also called itself `base`.
fn unsuffixed(definitions: &Map<String, Value>) -> anyhow::Result<()> {
    for name in definitions.keys() {
        let digits = name
            .char_indices()
            .rev()
            .take_while(|(_, c)| c.is_ascii_digit());
        for (at, _) in digits {
            let (Some(base), Some(n)) = (name.get(..at), name.get(at..)) else {
                continue;
            };
            let suffix = !n.starts_with('0') && n.parse::<u64>().is_ok_and(|n| n >= 2);
            anyhow::ensure!(
                !(suffix && definitions.contains_key(base)),
                "`{name}` is a second type named `{base}`: rename one with `#[schemars(rename)]`"
            );
        }
    }
    Ok(())
}

/// Drop the doc comments schemars copies in: they are written for this crate's
/// readers, and a doc edit should not make the committed schema stale.
fn strip_descriptions(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            map.remove("description");
            for (key, value) in map.iter_mut() {
                // Under these keys the names are fields or definitions, not
                // keywords, so a field called `description` survives.
                if key == "properties" || key == "$defs" {
                    if let Value::Object(fields) = value {
                        fields.values_mut().for_each(strip_descriptions);
                    }
                } else {
                    strip_descriptions(value);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

/// Move each `$ref` that has sibling keys into an `allOf`: json2ts drops such a
/// `$ref`. Runs after [`strip_descriptions`], so a doc alone is no sibling.
fn ref_siblings_into_all_of(defs: &mut Value) {
    let mut transform = RemoveRefSiblings::default();
    if let Value::Object(definitions) = defs {
        definitions
            .values_mut()
            .filter_map(|schema| <&mut Schema>::try_from(schema).ok())
            .for_each(|schema| transform.transform(schema));
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use schemars::JsonSchema;

    use super::*;

    // Only their schemas are read, so none is ever built.
    mod one {
        #[derive(schemars::JsonSchema)]
        #[expect(dead_code, reason = "a schema fixture, never built")]
        pub(super) struct Twin {
            pub(super) left: u32,
        }
    }

    mod other {
        #[derive(schemars::JsonSchema)]
        #[expect(dead_code, reason = "a schema fixture, never built")]
        pub(super) struct Twin {
            pub(super) right: String,
        }
    }

    #[derive(JsonSchema)]
    #[schemars(rename = "RegisterClerkUserKeyRequest")]
    #[expect(dead_code, reason = "a schema fixture, never built")]
    struct Impostor {
        token: bool,
    }

    #[test]
    fn test_the_bundle_has_no_name_two_types_share() -> anyhow::Result<()> {
        let bundle = bundle_with(|_| {})?;
        anyhow::ensure!(
            bundle
                .pointer("/$defs/RegisterClerkUserKeyRequest")
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn test_a_second_type_of_the_same_name_is_refused() {
        let refused = bundle_with(|generator| {
            generator.subschema_for::<one::Twin>();
            generator.subschema_for::<other::Twin>();
        });
        let message = refused.err().map(|e| e.to_string());
        assert_eq!(
            message.as_deref(),
            Some("`Twin2` is a second type named `Twin`: rename one with `#[schemars(rename)]`")
        );
    }

    #[test]
    fn test_a_response_shadowing_a_request_is_refused() {
        let refused = bundle_with(|generator| {
            generator.subschema_for::<Impostor>();
        });
        let message = refused.err().map(|e| e.to_string());
        assert_eq!(
            message.as_deref(),
            Some(
                "`RegisterClerkUserKeyRequest` names a request and a response with different schemas: \
                 rename one type, or give a type read both ways a request-side mirror"
            )
        );
    }

    #[test]
    fn test_a_name_ending_in_digits_alone_is_kept() -> anyhow::Result<()> {
        let mut definitions = Map::new();
        definitions.insert("Sha256".into(), Value::Bool(true));
        definitions.insert("Plan".into(), Value::Bool(true));
        definitions.insert("Plan02".into(), Value::Bool(true));
        unsuffixed(&definitions)
    }

    /// Every `$ref` in `value` that has a sibling key, by its path.
    fn refs_with_siblings(value: &Value, path: &str, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                if map.contains_key("$ref") && map.len() > 1 {
                    found.push(path.to_owned());
                }
                for (key, inner) in map {
                    refs_with_siblings(inner, &format!("{path}/{key}"), found);
                }
            }
            Value::Array(items) => {
                for (at, inner) in items.iter().enumerate() {
                    refs_with_siblings(inner, &format!("{path}/{at}"), found);
                }
            }
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    /// json2ts drops a `$ref` that has sibling keys, as a tagged model variant
    /// would have, so the bundle holds none.
    #[test]
    fn test_no_ref_has_a_sibling() -> anyhow::Result<()> {
        let bundle = bundle()?;
        anyhow::ensure!(
            bundle.pointer("/$defs/Model").is_some(),
            "the jobs' requested models are in the bundle"
        );
        let mut found = Vec::new();
        refs_with_siblings(&bundle, "", &mut found);
        anyhow::ensure!(found.is_empty(), "{found:?}");
        Ok(())
    }

    /// The committed bundle is what these types produce. `UPDATE_API_SCHEMA=1`
    /// rewrites it; then `npm run types` in the app regenerates its TypeScript.
    #[test]
    fn test_the_committed_schema_is_current() -> anyhow::Result<()> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(SCHEMA_PATH);
        let current = format!("{}\n", serde_json::to_string_pretty(&bundle()?)?);
        if std::env::var_os("UPDATE_API_SCHEMA").is_some() {
            std::fs::write(&path, &current)?;
        }
        let committed = std::fs::read_to_string(&path)?;
        assert!(
            committed == current,
            "{SCHEMA_PATH} is stale: run `UPDATE_API_SCHEMA=1 cargo test -p pipette-httpapi \
             --lib api_schema`, then `npm run types` in ui/"
        );
        Ok(())
    }
}
