//! A JSON Schema check for the keywords schemars writes into the OpenAPI
//! document. Not the `jsonschema` crate: its dozen dependencies would count as
//! shipped here. A keyword it does not know is a refusal, never a pass.

use regex::Regex;
use serde_json::Value;

/// Keywords that describe a value and constrain nothing.
const ANNOTATIONS: &[&str] = &[
    "$schema",
    "description",
    "default",
    "examples",
    "format",
    "title",
    "deprecated",
    "readOnly",
    "writeOnly",
];

/// What `schema` finds wrong with `instance`, each at its JSON pointer. A
/// `$ref` resolves against `document`.
pub(crate) fn refusals(document: &Value, schema: &Value, instance: &Value) -> Vec<String> {
    let mut found = Vec::new();
    check(document, schema, instance, "", &mut found);
    found
}

fn check(document: &Value, schema: &Value, instance: &Value, at: &str, found: &mut Vec<String>) {
    let schema = match schema {
        Value::Bool(true) => return,
        Value::Bool(false) => {
            found.push(format!("{at}: nothing is allowed here"));
            return;
        }
        Value::Object(schema) => schema,
        Value::Null | Value::Number(_) | Value::String(_) | Value::Array(_) => {
            found.push(format!("{at}: `{schema}` is not a schema"));
            return;
        }
    };
    schema
        .iter()
        .filter(|(keyword, _)| !ANNOTATIONS.contains(&keyword.as_str()))
        .for_each(|(keyword, value)| match keyword.as_str() {
            "$ref" => match value
                .as_str()
                .and_then(|reference| reference.strip_prefix('#'))
                .and_then(|pointer| document.pointer(pointer))
            {
                Some(target) => check(document, target, instance, at, found),
                None => found.push(format!("{at}: `{value}` resolves to nothing")),
            },
            "type" => {
                let allowed: Vec<&str> = match value {
                    Value::String(one) => vec![one.as_str()],
                    Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => {
                        Vec::new()
                    }
                };
                if !allowed.iter().any(|kind| is_kind(instance, kind)) {
                    found.push(format!("{at}: {} where {value} is wanted", kind(instance)));
                }
            }
            "const" => {
                if instance != value {
                    found.push(format!("{at}: {instance} where {value} is wanted"));
                }
            }
            "enum" => {
                if !value
                    .as_array()
                    .is_some_and(|options| options.contains(instance))
                {
                    found.push(format!("{at}: {instance} is none of {value}"));
                }
            }
            "properties" => {
                if let (Some(properties), Value::Object(fields)) = (value.as_object(), instance) {
                    properties
                        .iter()
                        .filter_map(|(name, property)| Some((name, property, fields.get(name)?)))
                        .for_each(|(name, property, field)| {
                            check(document, property, field, &format!("{at}/{name}"), found);
                        });
                }
            }
            "required" => {
                if let (Some(names), Value::Object(fields)) = (value.as_array(), instance) {
                    found.extend(
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .filter(|name| !fields.contains_key(*name))
                            .map(|name| format!("{at}: `{name}` is missing")),
                    );
                }
            }
            "additionalProperties" => {
                if let Value::Object(fields) = instance {
                    let listed = schema.get("properties").and_then(Value::as_object);
                    fields
                        .iter()
                        .filter(|(name, _)| {
                            !listed.is_some_and(|listed| listed.contains_key(*name))
                        })
                        .for_each(|(name, field)| {
                            check(document, value, field, &format!("{at}/{name}"), found);
                        });
                }
            }
            "items" => {
                if let Value::Array(items) = instance {
                    items.iter().enumerate().for_each(|(index, item)| {
                        check(document, value, item, &format!("{at}/{index}"), found);
                    });
                }
            }
            "allOf" | "anyOf" | "oneOf" => {
                let arms = value.as_array().map_or(&[][..], Vec::as_slice);
                let admitted = arms
                    .iter()
                    .filter(|arm| refusals(document, arm, instance).is_empty())
                    .count();
                let fits = match keyword.as_str() {
                    "allOf" => admitted == arms.len(),
                    "anyOf" => admitted >= 1,
                    _ => admitted == 1,
                };
                if !fits {
                    found.push(format!(
                        "{at}: {admitted} of the {} arms of `{keyword}` admit {}",
                        arms.len(),
                        kind(instance)
                    ));
                }
            }
            "minimum" | "maximum" => {
                if let (Some(bound), Some(number)) = (value.as_f64(), instance.as_f64()) {
                    let inside = if keyword == "minimum" {
                        number >= bound
                    } else {
                        number <= bound
                    };
                    if !inside {
                        found.push(format!("{at}: {number} is past the {keyword} {bound}"));
                    }
                }
            }
            "minLength" => {
                if let (Some(least), Some(text)) = (value.as_u64(), instance.as_str()) {
                    let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
                    if length < least {
                        found.push(format!("{at}: {instance} is shorter than {least}"));
                    }
                }
            }
            "maxLength" => {
                if let (Some(most), Some(text)) = (value.as_u64(), instance.as_str()) {
                    let length = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
                    if length > most {
                        found.push(format!("{at}: {instance} is longer than {most}"));
                    }
                }
            }
            "maxProperties" => {
                if let (Some(most), Value::Object(fields)) = (value.as_u64(), instance)
                    && u64::try_from(fields.len()).unwrap_or(u64::MAX) > most
                {
                    found.push(format!("{at}: more than {most} properties"));
                }
            }
            "propertyNames" => {
                if let Value::Object(fields) = instance {
                    fields.keys().for_each(|name| {
                        check(
                            document,
                            value,
                            &Value::String(name.clone()),
                            &format!("{at}/{name}"),
                            found,
                        );
                    });
                }
            }
            "minItems" => {
                if let (Some(least), Value::Array(items)) = (value.as_u64(), instance)
                    && u64::try_from(items.len()).unwrap_or(u64::MAX) < least
                {
                    found.push(format!("{at}: fewer than {least} items"));
                }
            }
            "uniqueItems" => {
                if let (Some(true), Value::Array(items)) = (value.as_bool(), instance) {
                    let repeated = items.iter().enumerate().any(|(index, item)| {
                        items.iter().skip(index + 1).any(|other| other == item)
                    });
                    if repeated {
                        found.push(format!("{at}: an item repeats"));
                    }
                }
            }
            "pattern" => {
                if let (Some(pattern), Some(text)) = (value.as_str(), instance.as_str()) {
                    match matches(pattern, text) {
                        Ok(true) => {}
                        Ok(false) => {
                            found.push(format!("{at}: {instance} does not match {pattern}"))
                        }
                        Err(e) => {
                            found.push(format!("{at}: the pattern {pattern} does not compile: {e}"))
                        }
                    }
                }
            }
            unknown => found.push(format!(
                "{at}: the check does not know the keyword `{unknown}`"
            )),
        });
}

/// Whether `text` holds a match for `pattern`, which JSON Schema leaves unanchored.
fn matches(pattern: &str, text: &str) -> Result<bool, regex::Error> {
    Ok(Regex::new(pattern)?.is_match(text))
}

fn is_kind(instance: &Value, wanted: &str) -> bool {
    match instance {
        Value::Number(number) if wanted == "integer" => {
            number.is_i64()
                || number.is_u64()
                || number.as_f64().is_some_and(|float| float.fract() == 0.0)
        }
        Value::Null
        | Value::Bool(_)
        | Value::Number(_)
        | Value::String(_)
        | Value::Array(_)
        | Value::Object(_) => kind(instance) == wanted,
    }
}

fn kind(instance: &Value) -> &'static str {
    match instance {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use pipette_httpapi::api_schema::openapi::OpenApi;

    use super::*;

    /// The keywords the check applies.
    const KEYWORDS: &[&str] = &[
        "$ref",
        "type",
        "const",
        "enum",
        "properties",
        "required",
        "additionalProperties",
        "items",
        "allOf",
        "anyOf",
        "oneOf",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "maxProperties",
        "propertyNames",
        "minItems",
        "uniqueItems",
        "pattern",
    ];

    #[test]
    fn test_each_keyword_case() -> anyhow::Result<()> {
        let object = json!({
            "type": "object",
            "properties": { "slug": { "type": "string" } },
            "required": ["slug"],
            "additionalProperties": false
        });
        let one_of = json!({ "oneOf": [{ "const": "a" }, { "type": "string" }] });
        let any_of = json!({ "anyOf": [{ "const": "a" }, { "type": "string" }] });
        // What each case shows, its schema, its instance, and whether the
        // check refuses it.
        let cases = [
            (
                "an integer is an integer",
                json!({ "type": "integer" }),
                json!(3),
                false,
            ),
            (
                "a fraction is no integer",
                json!({ "type": "integer" }),
                json!(3.5),
                true,
            ),
            (
                "an integer is a number",
                json!({ "type": "number" }),
                json!(3),
                false,
            ),
            (
                "null fits a nullable string",
                json!({ "type": ["string", "null"] }),
                json!(null),
                false,
            ),
            (
                "a number fits no nullable string",
                json!({ "type": ["string", "null"] }),
                json!(1),
                true,
            ),
            (
                "an object with its field",
                object.clone(),
                json!({ "slug": "acme" }),
                false,
            ),
            (
                "an object missing a required field",
                object.clone(),
                json!({}),
                true,
            ),
            (
                "a field of another type",
                object.clone(),
                json!({ "slug": 7 }),
                true,
            ),
            (
                "a field the object does not list",
                object,
                json!({ "slug": "acme", "extra": 1 }),
                true,
            ),
            (
                "oneOf takes a value one arm admits",
                one_of.clone(),
                json!("b"),
                false,
            ),
            (
                "oneOf refuses a value two arms admit",
                one_of,
                json!("a"),
                true,
            ),
            (
                "anyOf takes a value two arms admit",
                any_of,
                json!("a"),
                false,
            ),
            ("below the minimum", json!({ "minimum": 1 }), json!(0), true),
            ("above the maximum", json!({ "maximum": 1 }), json!(2), true),
            (
                "minLength counts characters",
                json!({ "minLength": 2 }),
                json!("é"),
                true,
            ),
            (
                "maxLength counts characters",
                json!({ "maxLength": 1 }),
                json!("é"),
                false,
            ),
            (
                "past the maxLength",
                json!({ "maxLength": 1 }),
                json!("ab"),
                true,
            ),
            (
                "more properties than maxProperties",
                json!({ "maxProperties": 1 }),
                json!({ "a": 1, "b": 2 }),
                true,
            ),
            (
                "a property name propertyNames refuses",
                json!({ "propertyNames": { "minLength": 2 } }),
                json!({ "ok": 1, "x": 2 }),
                true,
            ),
            (
                "a pattern that does not match",
                json!({ "pattern": "^v[0-9]+$" }),
                json!("x1"),
                true,
            ),
            (
                "a pattern matches anywhere",
                json!({ "pattern": "[0-9]" }),
                json!("v1"),
                false,
            ),
            (
                "fewer items than minItems",
                json!({ "minItems": 1 }),
                json!([]),
                true,
            ),
            (
                "as many items as minItems",
                json!({ "minItems": 1 }),
                json!([1]),
                false,
            ),
            (
                "a repeated item",
                json!({ "uniqueItems": true }),
                json!([1, 1]),
                true,
            ),
            (
                "a value the enum lists",
                json!({ "enum": ["a", "b"] }),
                json!("b"),
                false,
            ),
            (
                "a keyword the check does not know",
                json!({ "maxItems": 1 }),
                json!([]),
                true,
            ),
            (
                "annotations constrain nothing",
                json!({ "description": "x", "format": "uuid" }),
                json!("x"),
                false,
            ),
        ];
        let document = json!({});
        let wrong: Vec<&str> = cases
            .iter()
            .filter(|(_, schema, instance, refuses)| {
                refusals(&document, schema, instance).is_empty() == *refuses
            })
            .map(|(name, ..)| *name)
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }

    #[test]
    fn test_a_reference_resolves_against_the_document() {
        let document = json!({ "components": { "schemas": { "Id": { "type": "string" } } } });
        let schema = json!({ "$ref": "#/components/schemas/Id" });
        assert!(refusals(&document, &schema, &json!("x")).is_empty());
        assert!(!refusals(&document, &schema, &json!(1)).is_empty());
        assert!(!refusals(&document, &json!({ "$ref": "#/nowhere" }), &json!("x")).is_empty());
    }

    /// Each keyword in `schema`, or a schema under it, that `check` does not
    /// apply.
    fn unknown_keywords(schema: &Value, at: &str, found: &mut Vec<String>) {
        let Value::Object(schema) = schema else {
            return;
        };
        schema
            .iter()
            .for_each(|(keyword, value)| match keyword.as_str() {
                "properties" => {
                    value
                        .as_object()
                        .into_iter()
                        .flatten()
                        .for_each(|(name, property)| {
                            unknown_keywords(property, &format!("{at}/properties/{name}"), found);
                        })
                }
                "items" | "additionalProperties" | "propertyNames" => {
                    unknown_keywords(value, &format!("{at}/{keyword}"), found);
                }
                "allOf" | "anyOf" | "oneOf" => value
                    .as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .for_each(|(index, arm)| {
                        unknown_keywords(arm, &format!("{at}/{keyword}/{index}"), found);
                    }),
                known if KEYWORDS.contains(&known) || ANNOTATIONS.contains(&known) => {}
                unknown => found.push(format!("{at}: `{unknown}`")),
            });
    }

    /// [`unknown_keywords`] over every schema in the document: the components,
    /// and each `schema` an operation names.
    fn unknown_keywords_in(document: &Value, at: &str, found: &mut Vec<String>) {
        match document {
            Value::Object(fields) => fields.iter().for_each(|(key, value)| {
                let here = format!("{at}/{key}");
                if key == "schema" || at == "/components/schemas" {
                    unknown_keywords(value, &here, found);
                } else {
                    unknown_keywords_in(value, &here, found);
                }
            }),
            Value::Array(items) => items.iter().enumerate().for_each(|(index, item)| {
                unknown_keywords_in(item, &format!("{at}/{index}"), found);
            }),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    /// Every keyword in the served document is one the check applies, so
    /// nothing in it passes unchecked.
    #[test]
    fn test_the_document_uses_only_keywords_the_check_knows() -> anyhow::Result<()> {
        let openapi = OpenApi::build()?;
        let mut unknown = Vec::new();
        unknown_keywords_in(openapi.document(), "", &mut unknown);
        anyhow::ensure!(unknown.is_empty(), "{unknown:#?}");
        Ok(())
    }
}
