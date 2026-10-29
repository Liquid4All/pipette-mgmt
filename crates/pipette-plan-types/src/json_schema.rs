//! `JsonSchema` where the derive cannot write it: `nutype` 0.8 speaks only
//! schemars 0.8, the open flavor enums serialize by hand, and a flag group's
//! wire tags a variant by its cell's axis keys rather than by its name.

use std::borrow::Cow;
use std::fmt::Debug;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::Serialize;
use serde_json::{Map, Value, json};
use strum::IntoEnumIterator as _;

use crate::{
    AbsolutePath, AuditEventId, BenchmarkId, BenchmarkType, CampaignId, CapabilityFlag,
    CatalogName, ClientId, ClientPreauthKeyId, ContactEmail, DockerShmSize, ElapsedMillis,
    EnvForward, FileName, HfOrg, HfRepoName, HfRevision, JobId, Labels, LlamaCppFlavor, ModelKey,
    ModelType, NonEmptyString, NonEmptyTrimmedString, OnnxExecutionProvider, OpenvinoDevice, OrgId,
    OrgSlug, OvmsFlavor, PlanId, PositiveCount, PositiveSeconds, PublicKeyHex, RelativePath,
    RemoteArchiveUrl, RepoSubpath, RepositoryUrl, ResourceUrl, RuntimeType, SemverWithBuild,
    Sha256, SkinCelsius, Tag, TrtEdgellmFlavor, UserId, UserKeyId, UserPreauthKeyId, UvBuild,
    UvPythonVersion, UvServerVersion, VaultResourceId,
};

/// A newtype is its wire primitive, inlined: TS sees `string`, not an alias.
macro_rules! primitive_schema {
    ($schema:tt: $($ty:ident),+ $(,)?) => {$(
        impl JsonSchema for $ty {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> Cow<'static, str> {
                stringify!($ty).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                json_schema!($schema)
            }
        }
    )+};
}

primitive_schema!({ "type": "string" }:
    AbsolutePath, AuditEventId, BenchmarkId, CampaignId, ClientId, ClientPreauthKeyId, ContactEmail, DockerShmSize, EnvForward, FileName, HfOrg, HfRepoName, HfRevision, JobId, ModelKey, NonEmptyString, NonEmptyTrimmedString, OrgId, OrgSlug, PlanId, PublicKeyHex, RelativePath, RemoteArchiveUrl, RepoSubpath, RepositoryUrl, ResourceUrl, SemverWithBuild, Sha256, Tag, UserId, UserKeyId, UserPreauthKeyId, UvBuild, UvPythonVersion, UvServerVersion, VaultResourceId,
);
// The type also refuses non-ASCII uppercase, which the pattern does not check.
primitive_schema!({ "type": "string", "pattern": "^[^\\sA-Z]+$" }: CapabilityFlag);
primitive_schema!({ "type": "string", "pattern": "^[A-Za-z0-9](?: ?[A-Za-z0-9._-])*$", "maxLength": 200 }: CatalogName);
primitive_schema!({ "type": "integer", "minimum": 0 }: ElapsedMillis);
primitive_schema!({ "type": "integer", "minimum": 1 }: PositiveCount, PositiveSeconds);
primitive_schema!({ "type": "integer", "minimum": 1, "maximum": 110 }: SkinCelsius);
primitive_schema!({
    "type": "object",
    "maxProperties": Labels::MAX,
    "propertyNames": { "minLength": 1, "maxLength": Labels::KEY_MAX_CHARS },
    "additionalProperties": { "type": "string", "maxLength": Labels::VALUE_MAX_CHARS },
}: Labels);

/// An unknown spelling parses into `Custom`, so the schema is a non-empty
/// `string`, with the known spellings as examples.
macro_rules! open_string_schema {
    ($($ty:ident),+ $(,)?) => {$(
        impl JsonSchema for $ty {
            fn inline_schema() -> bool {
                true
            }
            fn schema_name() -> Cow<'static, str> {
                stringify!($ty).into()
            }
            fn json_schema(_: &mut SchemaGenerator) -> Schema {
                // `iter` yields `Custom` with an empty string, which is no spelling.
                let known: Vec<String> = $ty::iter()
                    .map(|v| v.as_str().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                json_schema!({ "type": "string", "minLength": 1, "examples": known })
            }
        }
    )+};
}

open_string_schema!(
    LlamaCppFlavor,
    OnnxExecutionProvider,
    OpenvinoDevice,
    OvmsFlavor,
    TrtEdgellmFlavor
);

/// The key the flag enums' `schemars(tag)` names; it must match those attributes.
const VARIANT_TAG: &str = "variant";

/// The transform for a flag enum whose variants are named `<Benchmark><Runtime><Model>`.
pub(crate) fn triple_arms(schema: &mut Schema) {
    cell_arms(schema, &[benchmark_axis(), runtime_axis(), model_axis()]);
}

/// The transform for a flag enum whose variants are named `<Benchmark><Model>`.
pub(crate) fn pair_arms(schema: &mut Schema) {
    cell_arms(schema, &[benchmark_axis(), model_axis()]);
}

/// A wire key and its axis enum's values, each by Rust name and wire spelling.
struct Axis {
    key: &'static str,
    values: Vec<(String, Value)>,
}

fn axis<T: Debug + Serialize>(key: &'static str, values: impl IntoIterator<Item = T>) -> Axis {
    let values = values
        .into_iter()
        .filter_map(|value| Some((format!("{value:?}"), serde_json::to_value(value).ok()?)))
        .collect();
    Axis { key, values }
}

fn benchmark_axis() -> Axis {
    axis("benchmark_type", BenchmarkType::ALL.iter())
}

fn runtime_axis() -> Axis {
    axis("runtime_type", RuntimeType::iter())
}

fn model_axis() -> Axis {
    axis("model_type", ModelType::iter())
}

/// Swaps each arm's name tag for its cell's axis keys, required alone as the wire
/// omits an unset knob. An arm naming no single cell keeps its tag, for the tests.
fn cell_arms(schema: &mut Schema, axes: &[Axis]) {
    let Some(arms) = schema.get_mut("oneOf").and_then(Value::as_array_mut) else {
        return;
    };
    for arm in arms {
        let Some(name) = arm
            .pointer(&format!("/properties/{VARIANT_TAG}/const"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let Ok([cell]) = <[_; 1]>::try_from(cells_named(name, axes)) else {
            continue;
        };
        let Some(arm) = arm.as_object_mut() else {
            continue;
        };
        if let Some(properties) = arm.get_mut("properties").and_then(Value::as_object_mut) {
            properties.remove(VARIANT_TAG);
            for (key, wire) in &cell {
                properties.insert(key.clone(), json!({ "type": "string", "const": wire }));
            }
        }
        // Sorted, as a map's order follows whether a build turns on
        // `serde_json/preserve_order`.
        let mut required: Vec<&String> = cell.keys().collect();
        required.sort();
        arm.insert("required".to_owned(), json!(required));
        arm.insert("additionalProperties".to_owned(), false.into());
    }
}

/// Every way `name` reads as one value of each axis in turn, nothing left over.
fn cells_named(name: &str, axes: &[Axis]) -> Vec<Map<String, Value>> {
    let Some((axis, rest)) = axes.split_first() else {
        return if name.is_empty() {
            vec![Map::new()]
        } else {
            Vec::new()
        };
    };
    axis.values
        .iter()
        .filter_map(|(ident, wire)| Some((name.strip_prefix(ident.as_str())?, wire)))
        .flat_map(|(tail, wire)| {
            cells_named(tail, rest).into_iter().map(|mut cell| {
                cell.insert(axis.key.to_owned(), wire.clone());
                cell
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) const TRIPLE: &[&str] = &["benchmark_type", "runtime_type", "model_type"];

#[cfg(test)]
pub(crate) const PAIR: &[&str] = &["benchmark_type", "model_type"];

/// `T`'s schema as mgmt generates it: what the type serializes to.
#[cfg(test)]
fn root<T: JsonSchema>() -> anyhow::Result<Value> {
    let schema = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .into_generator()
        .into_root_schema_for::<T>();
    Ok(serde_json::to_value(schema)?)
}

/// The arms of a flag group's schema.
#[cfg(test)]
fn arms(schema: &Value) -> anyhow::Result<&Vec<Value>> {
    use anyhow::Context;

    schema
        .pointer("/oneOf")
        .and_then(Value::as_array)
        .context("a flag group's schema is a `oneOf`")
}

/// An arm's axis consts, in `axis_keys` order.
#[cfg(test)]
fn arm_cell(arm: &Value, axis_keys: &[&str]) -> anyhow::Result<Vec<String>> {
    use anyhow::Context;

    axis_keys
        .iter()
        .map(|key| {
            arm.pointer(&format!("/properties/{key}/const"))
                .and_then(Value::as_str)
                .map(str::to_owned)
                .with_context(|| format!("an arm has no `{key}` const: {arm}"))
        })
        .collect()
}

/// Each arm of `Flags`' schema, by its axis consts in `axis_keys` order, with
/// the knobs it takes. Refuses an arm that requires more than its axis keys or
/// takes keys it does not list.
#[cfg(test)]
pub(crate) fn flag_arms<Flags: JsonSchema>(
    axis_keys: &[&str],
) -> anyhow::Result<std::collections::BTreeMap<Vec<String>, std::collections::BTreeSet<String>>> {
    use anyhow::Context;

    let schema = root::<Flags>()?;
    let mut sorted_keys = axis_keys.to_vec();
    sorted_keys.sort_unstable();
    let mut out = std::collections::BTreeMap::new();
    for arm in arms(&schema)? {
        let properties = arm
            .get("properties")
            .and_then(Value::as_object)
            .with_context(|| format!("an arm has no properties: {arm}"))?;
        anyhow::ensure!(
            arm.get("required") == Some(&json!(sorted_keys)),
            "an arm requires other than its axis keys: {arm}"
        );
        anyhow::ensure!(
            arm.get("additionalProperties") == Some(&Value::Bool(false)),
            "an arm is open to other keys: {arm}"
        );
        let axes = arm_cell(arm, axis_keys)?;
        let knobs = properties
            .keys()
            .filter(|key| !axis_keys.contains(&key.as_str()))
            .cloned()
            .collect();
        anyhow::ensure!(
            out.insert(axes.clone(), knobs).is_none(),
            "two arms for {axes:?}"
        );
    }
    Ok(out)
}

/// Asserts `Flags`' arms are `cells`, as `(benchmark, runtime, model, knobs)` rows.
#[cfg(test)]
pub(crate) fn assert_arms_are<Flags: JsonSchema>(
    cells: &[(&str, &str, &str, &[&str])],
) -> anyhow::Result<()> {
    let table: std::collections::BTreeMap<Vec<String>, std::collections::BTreeSet<String>> = cells
        .iter()
        .map(|(benchmark, runtime, model, knobs)| {
            let axes = [benchmark, runtime, model].map(|axis| (*axis).to_owned());
            (
                axes.to_vec(),
                knobs.iter().map(|knob| (*knob).to_owned()).collect(),
            )
        })
        .collect();
    let arms = flag_arms::<Flags>(TRIPLE)?;
    assert_eq!(arms, table);
    Ok(())
}

/// Asserts `Flags`' arms are the cells among `candidates` its deserializer
/// takes bare, each with the knobs of `probes` it takes on that cell. A probe
/// is a knob and a TOML value every cell carrying that knob accepts; the probes
/// must be the flat ref's knobs, so a new wire key cannot go untested.
#[cfg(test)]
pub(crate) fn assert_arms_are_accepted<Flags, Ref>(
    axis_keys: &[&str],
    candidates: impl IntoIterator<Item = Vec<String>>,
    probes: &[(&str, &str)],
) -> anyhow::Result<()>
where
    Flags: JsonSchema + serde::de::DeserializeOwned,
    Ref: JsonSchema,
{
    let flat: std::collections::BTreeSet<String> = root::<Ref>()?
        .pointer("/properties")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("the flat ref's schema has properties"))?
        .keys()
        .filter(|key| !axis_keys.contains(&key.as_str()))
        .cloned()
        .collect();
    let probed: std::collections::BTreeSet<String> =
        probes.iter().map(|(knob, _)| (*knob).to_owned()).collect();
    assert_eq!(probed, flat, "the probes against the flat ref's knobs");
    let mut accepted = std::collections::BTreeMap::new();
    for cell in candidates {
        let axes: String = axis_keys
            .iter()
            .zip(&cell)
            .map(|(key, value)| format!("{key} = \"{value}\"\n"))
            .collect();
        if toml::from_str::<Flags>(&axes).is_err() {
            continue;
        }
        let knobs: std::collections::BTreeSet<String> = probes
            .iter()
            .filter(|(knob, value)| {
                toml::from_str::<Flags>(&format!("{axes}{knob} = {value}\n")).is_ok()
            })
            .map(|(knob, _)| (*knob).to_owned())
            .collect();
        accepted.insert(cell, knobs);
    }
    anyhow::ensure!(!accepted.is_empty(), "no candidate cell parsed");
    assert_eq!(flag_arms::<Flags>(axis_keys)?, accepted);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use anyhow::Context;

    use super::*;
    use crate::{
        BenchmarkFlagRef, BenchmarkFlags, ClientRunSpec, Model, ModelFlagRef, ModelFlags, Plan,
        Runtime, RuntimeFlagRef, RuntimeFlags,
    };

    /// The `type` tag of each arm of `T`'s schema, sorted.
    fn tags<T: JsonSchema>() -> anyhow::Result<Vec<String>> {
        let schema = root::<T>()?;
        let mut tags = schema
            .pointer("/oneOf")
            .and_then(Value::as_array)
            .context("a tagged enum's schema is a `oneOf`")?
            .iter()
            .map(|arm| {
                arm.pointer("/properties/type/const")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .with_context(|| format!("an arm has no `type` const: {arm}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        tags.sort();
        Ok(tags)
    }

    #[test]
    fn model_has_an_arm_per_model_type() -> anyhow::Result<()> {
        let mut types: Vec<String> = ModelType::iter().map(|t| t.to_string()).collect();
        types.sort();
        assert_eq!(tags::<Model>()?, types);
        Ok(())
    }

    #[test]
    fn runtime_has_an_arm_per_runtime_type() -> anyhow::Result<()> {
        let mut types: Vec<String> = RuntimeType::iter().map(|t| t.to_string()).collect();
        types.sort();
        assert_eq!(tags::<Runtime>()?, types);
        Ok(())
    }

    /// Every property name anywhere in `value`, `$defs` included.
    fn property_names(value: &Value, names: &mut BTreeSet<String>) {
        match value {
            Value::Object(map) => {
                if let Some(properties) = map.get("properties").and_then(Value::as_object) {
                    names.extend(properties.keys().cloned());
                }
                map.values().for_each(|inner| property_names(inner, names));
            }
            Value::Array(items) => items.iter().for_each(|inner| property_names(inner, names)),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
        }
    }

    /// The generated types offer no token: the app never sees one.
    #[test]
    fn no_model_schema_has_an_auth_token() -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        property_names(&root::<Model>()?, &mut names);
        anyhow::ensure!(names.contains("repo_name"), "the walk missed `HfRepo`");
        anyhow::ensure!(!names.contains("auth_token"), "{names:?}");
        Ok(())
    }

    /// Every flag arm was renamed for its cell: the derive's name tag is gone
    /// from the whole spec, and `flag_arms` finds each arm's axis consts.
    #[test]
    fn every_flag_arm_is_tagged_by_its_cell() -> anyhow::Result<()> {
        let mut names = BTreeSet::new();
        property_names(&root::<ClientRunSpec>()?, &mut names);
        anyhow::ensure!(names.contains("runtime_type"), "the walk missed the flags");
        anyhow::ensure!(!names.contains(VARIANT_TAG), "{names:?}");
        flag_arms::<RuntimeFlags>(TRIPLE)?;
        flag_arms::<BenchmarkFlags>(TRIPLE)?;
        flag_arms::<ModelFlags>(PAIR)?;
        Ok(())
    }

    /// Every name a cell could give a variant reads as that cell alone, though
    /// one axis value's name may begin another's (`GgufVision`, `GgufVisionSpeculative`).
    #[test]
    fn a_cell_name_reads_as_that_cell_alone() -> anyhow::Result<()> {
        let triple = [benchmark_axis(), runtime_axis(), model_axis()];
        let pair = [benchmark_axis(), model_axis()];
        for &b in BenchmarkType::ALL {
            for m in ModelType::iter() {
                let cell = |runtime: Option<RuntimeType>| -> anyhow::Result<Map<String, Value>> {
                    let mut cell = Map::new();
                    cell.insert("benchmark_type".to_owned(), serde_json::to_value(b)?);
                    if let Some(r) = runtime {
                        cell.insert("runtime_type".to_owned(), serde_json::to_value(r)?);
                    }
                    cell.insert("model_type".to_owned(), serde_json::to_value(m)?);
                    Ok(cell)
                };
                assert_eq!(
                    cells_named(&format!("{b:?}{m:?}"), &pair),
                    vec![cell(None)?]
                );
                for r in RuntimeType::iter() {
                    let name = format!("{b:?}{r:?}{m:?}");
                    assert_eq!(cells_named(&name, &triple), vec![cell(Some(r))?], "{name}");
                    anyhow::ensure!(cells_named(&format!("{name}X"), &triple).is_empty());
                }
            }
        }
        Ok(())
    }

    /// `schema` with every `description` dropped: a doc edit is no type change.
    fn undocumented(schema: &Value) -> Value {
        match schema {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(key, _)| *key != "description")
                    .map(|(key, value)| (key.clone(), undocumented(value)))
                    .collect(),
            ),
            Value::Array(items) => Value::Array(items.iter().map(undocumented).collect()),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => schema.clone(),
        }
    }

    /// Each arm's knob takes what the flat ref takes under that key, so the
    /// variants' field types say what the wire accepts.
    fn knobs_take_the_flat_refs_values<Flags: JsonSchema, Ref: JsonSchema>(
        axis_keys: &[&str],
    ) -> anyhow::Result<()> {
        let flat = root::<Ref>()?;
        let flat = flat
            .get("properties")
            .and_then(Value::as_object)
            .context("a flat ref is an object")?;
        let schema = root::<Flags>()?;
        for arm in arms(&schema)? {
            let cell = arm_cell(arm, axis_keys)?;
            let properties = arm
                .get("properties")
                .and_then(Value::as_object)
                .context("an arm is an object")?;
            for (knob, knob_schema) in properties {
                if axis_keys.contains(&knob.as_str()) {
                    continue;
                }
                let wire = flat
                    .get(knob)
                    .with_context(|| format!("{cell:?}: `{knob}` is no wire key"))?;
                assert_eq!(
                    undocumented(knob_schema),
                    undocumented(wire),
                    "{cell:?}: `{knob}`"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn every_knob_takes_what_the_wire_takes() -> anyhow::Result<()> {
        knobs_take_the_flat_refs_values::<RuntimeFlags, RuntimeFlagRef>(TRIPLE)?;
        knobs_take_the_flat_refs_values::<BenchmarkFlags, BenchmarkFlagRef>(TRIPLE)?;
        knobs_take_the_flat_refs_values::<ModelFlags, ModelFlagRef>(PAIR)
    }

    /// The JSON types a property schema's `type` admits, if it names any.
    fn admitted_types(schema: &Value) -> Option<Vec<&str>> {
        match schema.get("type")? {
            Value::String(one) => Some(vec![one.as_str()]),
            Value::Array(many) => Some(many.iter().filter_map(Value::as_str).collect()),
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => None,
        }
    }

    fn json_type(value: &Value) -> &'static str {
        match value {
            Value::Null => "null",
            Value::Bool(_) => "boolean",
            Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }

    /// `entry` as the wire carries it fits the arm its axis keys name: its
    /// keys are that arm's, and each value is of a type the arm admits.
    fn fits_its_arm<Flags: JsonSchema + Serialize>(
        entry: &Flags,
        axis_keys: &[&str],
    ) -> anyhow::Result<()> {
        let wire = serde_json::to_value(entry)?;
        let fields = wire.as_object().context("a flag entry is an object")?;
        let cell = axis_keys
            .iter()
            .map(|key| {
                fields
                    .get(*key)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .with_context(|| format!("no `{key}` in {wire}"))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let schema = root::<Flags>()?;
        let arm = arms(&schema)?
            .iter()
            .find(|arm| arm_cell(arm, axis_keys).is_ok_and(|axes| axes == cell))
            .with_context(|| format!("no arm for {wire}"))?;
        let properties = arm
            .get("properties")
            .and_then(Value::as_object)
            .context("an arm is an object")?;
        for (key, value) in fields {
            let property = properties
                .get(key)
                .with_context(|| format!("`{key}` of {wire} is not on its arm"))?;
            if let Some(types) = admitted_types(property) {
                let found = json_type(value);
                anyhow::ensure!(
                    types.contains(&found) || (found == "integer" && types.contains(&"number")),
                    "`{key}` of {wire} is a {found}, its arm takes {types:?}"
                );
            }
        }
        Ok(())
    }

    /// The flag entries of the example plans, as served, fit their arms.
    #[test]
    fn example_plan_flags_fit_their_arms() -> anyhow::Result<()> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples/plans");
        let (mut runtime, mut benchmark, mut model) = (0, 0, 0);
        for path in std::fs::read_dir(&dir)? {
            let path = path?.path();
            if path.extension().is_none_or(|ext| ext != "toml") {
                continue;
            }
            let cells = Plan::load(&path)?.runnable_cells()?;
            for spec in cells.iter().map(ClientRunSpec::from) {
                let context = || format!("{}", path.display());
                if let Some(flags) = &spec.runtime_flags {
                    fits_its_arm(flags, TRIPLE).with_context(context)?;
                    runtime += 1;
                }
                if let Some(flags) = &spec.benchmark_flags {
                    fits_its_arm(flags, TRIPLE).with_context(context)?;
                    benchmark += 1;
                }
                if let Some(flags) = &spec.model_flags {
                    fits_its_arm(flags, PAIR).with_context(context)?;
                    model += 1;
                }
            }
        }
        anyhow::ensure!(
            runtime > 0 && benchmark > 0 && model > 0,
            "flag entries: {runtime} runtime, {benchmark} benchmark, {model} model"
        );
        Ok(())
    }

    /// The flag groups are named for their enums; the flat refs stay out of
    /// the generated types.
    #[test]
    fn a_spec_names_no_flat_ref() -> anyhow::Result<()> {
        let schema = root::<ClientRunSpec>()?;
        let defs = schema
            .get("$defs")
            .and_then(Value::as_object)
            .context("a spec's schema has definitions")?;
        for group in ["RuntimeFlags", "BenchmarkFlags", "ModelFlags"] {
            anyhow::ensure!(
                defs.contains_key(group),
                "no `{group}` in {:?}",
                defs.keys()
            );
        }
        anyhow::ensure!(
            !defs.keys().any(|name| name.ends_with("FlagRef")),
            "{:?}",
            defs.keys()
        );
        Ok(())
    }
}
