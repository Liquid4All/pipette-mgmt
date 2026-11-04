//! `catalog coverage`: how many recent results the descriptor catalog names,
//! and why it does not name the others. The report reads the warehouse and the
//! catalog, and writes nothing.
//!
//! The report counts results, not metric rows: all metric rows of one result
//! have the same descriptors. It classifies the runtime and the model of each
//! result apart. The first class that applies wins:
//!
//! 1. **exact:** the stored digest is the digest of a live member.
//! 2. **alias:** a verified alias would name it ([`crate::descriptor_alias`]).
//! 3. **unnamed,** with the first cause that applies ([`Cause`]).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use pipette_core::warehouse::MetricRow;
use pipette_plan_types::{
    DescriptorEntry, FailureSource, Model, Runtime, drop_vault_custom_labels,
};
use pipette_store::{
    DRAIN_PAGE_SIZE, DescriptorCatalogStore, DescriptorEntryFilter, OrgStore, Stores,
};

use crate::descriptor_alias::{AliasVerdict, alias_verdict};
use crate::descriptor_catalog_admin;
use crate::descriptor_names::CatalogReadScope;

/// Why the catalog does not name a descriptor. The order of the variants is
/// the order in which the report checks the causes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cause {
    /// The result has no descriptor of this kind.
    NoDescriptor,
    /// The server retired the job with no client run, so the descriptor comes
    /// from the raw job spec.
    SyntheticFailure,
    /// The descriptor does not parse as the typed descriptor.
    Unparsed,
    /// The typed value is equal to a member, but the descriptor has a key that
    /// the typed descriptor does not know.
    UnknownKey,
    /// No live member has this build.
    NoMember,
}

impl Cause {
    pub fn label(self) -> &'static str {
        match self {
            Self::NoDescriptor => "no descriptor",
            Self::SyntheticFailure => "synthetic failure",
            Self::Unparsed => "does not parse",
            Self::UnknownKey => "unknown key blocks the alias",
            Self::NoMember => "no member has this build",
        }
    }
}

/// The class of one descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Class {
    Exact,
    /// The member that the alias would name, as `scope/name::key`.
    Alias(String),
    Unnamed(Cause),
}

/// The live members of the catalog, in the forms that the report matches.
#[derive(Debug, Default)]
pub struct CoverageIndex {
    runtime_digests: HashSet<String>,
    /// The digests of model members, and the alias digests of vault models.
    model_digests: HashSet<String>,
    /// Keyed by the runtime with its vault `custom_label`s dropped.
    runtimes: HashMap<Runtime, String>,
    /// Keyed by the model with its vault `custom_label`s dropped.
    models: HashMap<Model, String>,
}

impl CoverageIndex {
    /// The index of `entries`. A tombstoned entry must not be in `entries`.
    pub fn new(entries: &[DescriptorEntry]) -> Self {
        let mut index = Self::default();
        entries.iter().for_each(|entry| {
            entry.members.iter().for_each(|member| {
                let label = format!(
                    "{}/{}::{}",
                    entry.scope,
                    entry.name,
                    descriptor_catalog_admin::member_key(member)
                );
                if let Some(runtime) = member.runtime() {
                    index.runtime_digests.insert(member.digest().to_owned());
                    if let Some(runtime) = unlabelled(runtime) {
                        index.runtimes.entry(runtime).or_insert(label);
                    }
                } else if let Some(model) = member.model() {
                    index.model_digests.insert(member.digest().to_owned());
                    index
                        .model_digests
                        .extend(member.alias_digest().map(str::to_owned));
                    // A member that does not survive the round trip only loses
                    // its aliases, so the report can only count fewer of them.
                    if let Some(model) = unlabelled(model) {
                        index.models.entry(model).or_insert(label);
                    }
                }
            });
        });
        index
    }
}

/// `value` with its vault `custom_label`s dropped, as the digest drops them.
fn unlabelled<T: Serialize + DeserializeOwned>(value: &T) -> Option<T> {
    let mut json = serde_json::to_value(value).ok()?;
    drop_vault_custom_labels(&mut json);
    serde_json::from_value(json).ok()
}

/// Every live entry (pinned, active or hidden) of every scope, and the number
/// of entries that the store could not read.
pub async fn load_live_entries(
    catalog: &dyn DescriptorCatalogStore,
    orgs: &dyn OrgStore,
) -> anyhow::Result<(Vec<DescriptorEntry>, usize)> {
    let filter = DescriptorEntryFilter {
        scopes: CatalogReadScope::EveryOrg { own_org: None }
            .readable_scopes(orgs)
            .await?,
        ..Default::default()
    };
    let mut entries = Vec::new();
    let mut unreadable = 0;
    let mut after = None;
    loop {
        let page = descriptor_catalog_admin::list(
            catalog,
            filter.clone(),
            after.as_ref(),
            DRAIN_PAGE_SIZE,
        )
        .await?;
        entries.extend(page.items);
        unreadable += page.unreadable.len();
        page.unreadable.iter().for_each(|entry| {
            tracing::warn!(%entry, "catalog coverage skips an unreadable catalog entry");
        });
        match page.resume_after {
            Some(next) => after = Some(next),
            None => return Ok((entries, unreadable)),
        }
    }
}

/// One descriptor digest that the catalog does not name exactly, with the
/// results that show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotExact {
    pub results: usize,
    /// Unnamed results by cause. The same digest can appear in client and
    /// synthetic results, which have different causes.
    pub causes: BTreeMap<Cause, usize>,
    /// Results that a verified alias would name.
    pub alias_results: usize,
    /// The member that an alias would name.
    pub alias_member: Option<String>,
    /// The newest `submitted_at` of the results, in microseconds since the
    /// epoch.
    pub newest: i64,
    /// The descriptor as one result stores it.
    pub sample: String,
}

/// The coverage of one kind of descriptor.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct KindCoverage {
    pub exact: usize,
    pub alias: usize,
    pub unnamed: BTreeMap<Cause, usize>,
    /// Keyed by the stored digest. A result with no descriptor is not here.
    pub not_exact: HashMap<String, NotExact>,
}

impl KindCoverage {
    pub fn total(&self) -> usize {
        self.exact + self.alias + self.unnamed_total()
    }

    pub fn unnamed_total(&self) -> usize {
        self.unnamed.values().sum()
    }

    /// The `n` digests with the most results, then the newest first.
    pub fn top(&self, n: usize) -> Vec<(&String, &NotExact)> {
        let mut top: Vec<_> = self.not_exact.iter().collect();
        top.sort_by(|(a_digest, a), (b_digest, b)| {
            (b.results, b.newest, a_digest).cmp(&(a.results, a.newest, b_digest))
        });
        top.truncate(n);
        top
    }

    fn record(&mut self, class: Class, digest: Option<&str>, descriptor: Option<&str>, at: i64) {
        let (cause, alias_member) = match class {
            Class::Exact => {
                self.exact += 1;
                return;
            }
            Class::Alias(member) => {
                self.alias += 1;
                (None, Some(member))
            }
            Class::Unnamed(cause) => {
                *self.unnamed.entry(cause).or_default() += 1;
                (Some(cause), None)
            }
        };
        let (Some(digest), Some(descriptor)) = (digest, descriptor) else {
            return;
        };
        let line = self
            .not_exact
            .entry(digest.to_owned())
            .or_insert_with(|| NotExact {
                results: 0,
                causes: BTreeMap::new(),
                alias_results: 0,
                alias_member: None,
                newest: at,
                sample: descriptor.to_owned(),
            });
        line.results += 1;
        if let Some(cause) = cause {
            *line.causes.entry(cause).or_default() += 1;
        } else {
            line.alias_results += 1;
            line.alias_member = alias_member;
        }
        line.newest = line.newest.max(at);
    }
}

/// The `catalog coverage` report.
#[derive(Debug, PartialEq, Eq)]
pub struct CoverageReport {
    pub since: DateTime<Utc>,
    /// Results in the window. Each kind counts each of them once.
    pub results: usize,
    pub runtime: KindCoverage,
    pub model: KindCoverage,
    /// Catalog entries that the store could not read. A row that one of them
    /// names counts as unnamed.
    pub unreadable_entries: usize,
}

impl CoverageReport {
    fn new(since: DateTime<Utc>, unreadable_entries: usize) -> Self {
        Self {
            since,
            results: 0,
            runtime: KindCoverage::default(),
            model: KindCoverage::default(),
            unreadable_entries,
        }
    }

    fn record_row(&mut self, index: &CoverageIndex, seen: &mut HashSet<String>, row: &MetricRow) {
        let job_id = row
            .result_id
            .rsplit_once('_')
            .filter(|(_, metric_index)| metric_index.parse::<usize>().is_ok())
            .map_or(row.result_id.as_str(), |(job_id, _)| job_id);
        if row.submitted_at < self.since.timestamp_micros() || !seen.insert(job_id.to_owned()) {
            return;
        }
        self.results += 1;
        let synthetic = row.failed == Some(true)
            && row
                .failure_source
                .is_some_and(|source| source != FailureSource::Client);
        let runtime = classify(
            row.runtime_descriptor.as_deref(),
            row.runtime_descriptor_sha256.as_deref(),
            synthetic,
            &index.runtime_digests,
            &index.runtimes,
        );
        self.runtime.record(
            runtime,
            row.runtime_descriptor_sha256.as_deref(),
            row.runtime_descriptor.as_deref(),
            row.submitted_at,
        );
        let model = classify(
            row.model_descriptor.as_deref(),
            row.model_descriptor_sha256.as_deref(),
            synthetic,
            &index.model_digests,
            &index.models,
        );
        self.model.record(
            model,
            row.model_descriptor_sha256.as_deref(),
            row.model_descriptor.as_deref(),
            row.submitted_at,
        );
    }

    /// The report as text, with the `top` digests of each kind that the catalog
    /// does not name exactly.
    pub fn render(&self, top: usize) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "catalog coverage: {} results submitted since {}",
            self.results,
            self.since.to_rfc3339()
        );
        if self.unreadable_entries > 0 {
            let _ = writeln!(
                out,
                "warning: {} catalog entries were unreadable; rows that they name count as unnamed",
                self.unreadable_entries
            );
        }
        [("runtimes", &self.runtime), ("models", &self.model)]
            .into_iter()
            .for_each(|(heading, kind)| render_kind(&mut out, heading, kind, top));
        out
    }

    pub fn log(&self, days: u32) {
        tracing::info!(
            days,
            results = self.results,
            runtime_exact = self.runtime.exact,
            runtime_alias = self.runtime.alias,
            runtime_unnamed = self.runtime.unnamed_total(),
            model_exact = self.model.exact,
            model_alias = self.model.alias,
            model_unnamed = self.model.unnamed_total(),
            unreadable_entries = self.unreadable_entries,
            "catalog coverage computed"
        );
    }
}

fn render_kind(out: &mut String, heading: &str, kind: &KindCoverage, top: usize) {
    let total = kind.total();
    let line = |out: &mut String, label: &str, count: usize| {
        let _ = writeln!(out, "  {label:<32} {count:>8}  {:>5}", share(count, total));
    };
    let _ = writeln!(out, "\n{heading}:");
    line(out, "named by an exact match", kind.exact);
    line(out, "an alias would name", kind.alias);
    line(out, "unnamed", kind.unnamed_total());
    kind.unnamed
        .iter()
        .for_each(|(cause, count)| line(out, &format!("  {}", cause.label()), *count));
    let top = kind.top(top);
    if top.is_empty() {
        return;
    }
    let _ = writeln!(out, "  most results without an exact match:");
    top.into_iter().for_each(|(digest, unnamed)| {
        let mut why = Vec::new();
        if let Some(member) = &unnamed.alias_member {
            why.push(format!("alias to {member}: {}", unnamed.alias_results));
        }
        why.extend(
            unnamed
                .causes
                .iter()
                .map(|(cause, count)| format!("{}: {count}", cause.label())),
        );
        let why = why.join(", ");
        let newest = DateTime::<Utc>::from_timestamp_micros(unnamed.newest)
            .map(|at| at.format("%Y-%m-%d").to_string())
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "  {:>8}  {}  {newest}  {why}\n            {}",
            unnamed.results,
            pipette_plan_types::short_digest(digest),
            unnamed.sample
        );
    });
}

/// `count` as a percentage of `total`, with one decimal.
fn share(count: usize, total: usize) -> String {
    if total == 0 {
        return "-".to_owned();
    }
    let percent = count as f64 * 100.0 / total as f64;
    format!("{percent:.1}%")
}

fn classify<T>(
    descriptor: Option<&str>,
    digest: Option<&str>,
    synthetic: bool,
    digests: &HashSet<String>,
    members: &HashMap<T, String>,
) -> Class
where
    T: DeserializeOwned + Serialize + Eq + std::hash::Hash,
{
    let Some(descriptor) = descriptor else {
        return Class::Unnamed(Cause::NoDescriptor);
    };
    if digest.is_some_and(|digest| digests.contains(digest)) {
        return Class::Exact;
    }
    let value = serde_json::from_str::<Value>(descriptor)
        .ok()
        .map(|mut value| {
            drop_vault_custom_labels(&mut value);
            value
        });
    let verdict = value.as_ref().map_or(AliasVerdict::Unparsed, |value| {
        alias_verdict(value, members)
    });
    match verdict {
        AliasVerdict::Alias(member) => Class::Alias(member.clone()),
        _ if synthetic => Class::Unnamed(Cause::SyntheticFailure),
        AliasVerdict::Unparsed => Class::Unnamed(Cause::Unparsed),
        AliasVerdict::Blocked { .. } => Class::Unnamed(Cause::UnknownKey),
        AliasVerdict::NoMember => Class::Unnamed(Cause::NoMember),
    }
}

/// Classify every result that was submitted at or after `since`. The visitor
/// marks no row dirty, so the walk needs no mutate lock.
pub async fn coverage(
    stores: &Stores,
    index: &CoverageIndex,
    since: DateTime<Utc>,
    unreadable_entries: usize,
) -> anyhow::Result<CoverageReport> {
    let mut report = CoverageReport::new(since, unreadable_entries);
    let mut seen = HashSet::new();
    stores
        .warehouse
        .for_each_metric_row(&mut |row| {
            report.record_row(index, &mut seen, row);
            false
        })
        .await?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::PermissionsExt;

    use anyhow::Context;
    use serde_json::json;

    use pipette_core::parquet_utils::WriterOpts;
    use pipette_core::warehouse;
    use pipette_plan_types::{
        BenchmarkId, CatalogScope, ClientId, DescriptorEntryKind, DescriptorEntryState,
        DescriptorMember, NonEmptyVec, OrgId, descriptor_digest,
    };
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::{a_descriptor_entry, a_member, a_model_member};

    use super::*;

    /// The window starts at 1 000 000 µs.
    const SINCE: i64 = 1_000_000;

    fn since() -> anyhow::Result<DateTime<Utc>> {
        DateTime::from_timestamp_micros(SINCE).context("a valid time")
    }

    /// The canonical string and the digest of `value`, as the server stores
    /// them.
    fn stored(value: &Value) -> anyhow::Result<(String, String)> {
        let mut identity = value.clone();
        drop_vault_custom_labels(&mut identity);
        Ok((
            pipette_plan_types::canonical_json(value),
            descriptor_digest(&identity)?,
        ))
    }

    fn runtime(version: &str) -> Value {
        json!({
            "type": "llamacpp_cli_stock_tools", "source": "github_release",
            "repository_url": "github.com/ggml-org/llama.cpp",
            "repository_version": version, "flavor": "linux-x64-rocm",
        })
    }

    fn model(repo: &str) -> Value {
        json!({
            "type": "gguf_text", "source": "huggingface", "org": "acme",
            "repo_name": repo, "path": "model.gguf",
            "revision": "0123456789abcdef0123456789abcdef01234567",
        })
    }

    /// A vault runtime of the organization `org_1`, under `label`.
    fn vault_runtime(resource: &str, label: &str) -> Value {
        json!({
            "type": "llamacpp_cli_stock_tools", "source": "vault_remote_archive",
            "url": format!("/vault/resources/{resource}/content"), "principal_id": "org_1",
            "custom_label": label, "flavor": "linux-x64-rocm",
        })
    }

    fn with(mut base: Value, key: &str, value: Value) -> Value {
        if let Some(fields) = base.as_object_mut() {
            fields.insert(key.to_owned(), value);
        }
        base
    }

    fn row(
        result_id: &str,
        runtime: Option<&Value>,
        model: Option<&Value>,
        submitted_at: i64,
    ) -> anyhow::Result<MetricRow> {
        let runtime = runtime.map(stored).transpose()?;
        let model = model.map(stored).transpose()?;
        Ok(MetricRow {
            result_id: result_id.to_owned(),
            benchmark_id: BenchmarkId::try_new("bench1")?,
            metric: "ttft".to_owned(),
            client_id: ClientId::try_new("c1")?,
            runtime_descriptor: runtime.as_ref().map(|(text, _)| text.clone()),
            runtime_descriptor_sha256: runtime.map(|(_, digest)| digest),
            model_descriptor: model.as_ref().map(|(text, _)| text.clone()),
            model_descriptor_sha256: model.map(|(_, digest)| digest),
            value: Some(1.0),
            unit: "ms".to_owned(),
            submitted_at,
            scored_at: submitted_at,
            ..MetricRow::default()
        })
    }

    fn index() -> anyhow::Result<CoverageIndex> {
        let mut hidden = a_descriptor_entry(
            "llamacpp-b4",
            DescriptorEntryKind::Stable,
            4,
            a_member("b4", "linux-x64-rocm")?,
        )?;
        hidden.state = DescriptorEntryState::Hidden;
        let mut fork = a_descriptor_entry(
            "fork",
            DescriptorEntryKind::HandMade,
            3,
            a_member("b1", "linux-x64-rocm")?,
        )?;
        fork.scope = CatalogScope::Org(OrgId::try_new("org_1")?);
        fork.members = NonEmptyVec::try_new(vec![DescriptorMember::new(
            serde_json::from_value(vault_runtime("vault_1", "nightly"))?,
            Utc::now(),
        )?])?;
        Ok(CoverageIndex::new(&[
            fork,
            a_descriptor_entry(
                "llamacpp-b1",
                DescriptorEntryKind::Stable,
                1,
                a_member("b1", "linux-x64-rocm")?,
            )?,
            hidden,
            a_descriptor_entry(
                "AcmeLM",
                DescriptorEntryKind::HandMade,
                2,
                a_model_member("gguf_text", "AcmeLM-GGUF", "q4_0")?,
            )?,
        ]))
    }

    fn rows() -> anyhow::Result<Vec<MetricRow>> {
        let exact = runtime("b1");
        let acme = model("AcmeLM-GGUF");
        let mut rows = vec![
            // One result with three metric rows counts once.
            row("exact_0", Some(&exact), Some(&acme), SINCE)?,
            row("exact_1", Some(&exact), Some(&acme), SINCE)?,
            row("exact_2", Some(&exact), Some(&acme), SINCE)?,
            row(
                "alias",
                Some(&with(exact.clone(), "asset", Value::Null)),
                Some(&with(acme.clone(), "sha256", Value::Null)),
                SINCE + 1,
            )?,
            row(
                "blocked",
                Some(&with(exact.clone(), "zzz", json!(1))),
                Some(&with(acme.clone(), "zzz", json!(1))),
                SINCE + 2,
            )?,
            row(
                "no-member-a",
                Some(&runtime("b2")),
                Some(&model("OtherLM")),
                SINCE + 3,
            )?,
            row(
                "no-member-b",
                Some(&runtime("b2")),
                Some(&model("OtherLM")),
                SINCE + 4,
            )?,
            row(
                "unparsed",
                Some(&json!({"type": "partner"})),
                None,
                SINCE + 5,
            )?,
            // A digest from before the label left it: only the alias names it.
            MetricRow {
                runtime_descriptor_sha256: Some("0".repeat(64)),
                ..row(
                    "vault-alias",
                    Some(&vault_runtime("vault_1", "renamed")),
                    None,
                    SINCE + 6,
                )?
            },
            row(
                "vault-no-member",
                Some(&vault_runtime("vault_2", "nightly")),
                None,
                SINCE + 6,
            )?,
            row("hidden", Some(&runtime("b4")), None, SINCE + 7)?,
            row("old", Some(&exact), Some(&acme), SINCE - 1)?,
        ];
        rows.push(MetricRow {
            failed: Some(true),
            failure_source: Some(FailureSource::Expired),
            ..row("synthetic", Some(&runtime("b3")), None, SINCE + 8)?
        });
        Ok(rows)
    }

    #[test]
    fn mixed_causes_for_one_digest_keep_their_counts_in_either_order() -> anyhow::Result<()> {
        for causes in [
            [Cause::NoMember, Cause::SyntheticFailure],
            [Cause::SyntheticFailure, Cause::NoMember],
        ] {
            let mut kind = KindCoverage::default();
            for cause in causes {
                kind.record(Class::Unnamed(cause), Some("digest"), Some("{}"), SINCE);
            }
            let line = kind.not_exact.get("digest").context("recorded digest")?;
            assert_eq!(line.results, 2);
            assert_eq!(
                line.causes,
                BTreeMap::from([(Cause::NoMember, 1), (Cause::SyntheticFailure, 1)])
            );
            let mut output = String::new();
            render_kind(&mut output, "runtime", &kind, 1);
            assert!(output.contains("no member has this build: 1"), "{output}");
            assert!(output.contains("synthetic failure: 1"), "{output}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn the_report_classifies_each_result_once_and_writes_nothing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..pipette_core::config::Config::default()
        };
        let stores = build_local_fs_stores(&config)?;
        let partition = dir
            .path()
            .join("warehouse/results/benchmark_id=bench1/client_id=c1/month=2026-10");
        std::fs::create_dir_all(&partition)?;
        std::fs::write(
            partition.join("part-0001.parquet"),
            warehouse::rows_to_parquet_bytes(WriterOpts::default(), &rows()?)?,
        )?;

        let original = std::fs::metadata(&partition)?.permissions();
        std::fs::set_permissions(&partition, std::fs::Permissions::from_mode(0o555))?;
        let report = coverage(&stores, &index()?, since()?, 0).await;
        std::fs::set_permissions(&partition, original)?;
        let report = report?;

        assert_eq!(report.results, 10, "the old result is outside the window");
        assert_eq!(
            (report.runtime.exact, report.runtime.alias),
            (2, 2),
            "a hidden entry still names a result"
        );
        assert_eq!(
            report.runtime.unnamed,
            BTreeMap::from([
                (Cause::SyntheticFailure, 1),
                (Cause::Unparsed, 1),
                (Cause::UnknownKey, 1),
                (Cause::NoMember, 3),
            ])
        );
        assert_eq!((report.model.exact, report.model.alias), (1, 1));
        assert_eq!(
            report.model.unnamed,
            BTreeMap::from([
                (Cause::NoDescriptor, 5),
                (Cause::UnknownKey, 1),
                (Cause::NoMember, 2),
            ])
        );

        let top = report.runtime.top(1);
        let (_, first) = top.first().context("one unnamed digest")?;
        assert_eq!(first.results, 2);
        assert_eq!(first.causes, BTreeMap::from([(Cause::NoMember, 2)]));
        assert_eq!(first.newest, SINCE + 4);
        let aliases: BTreeSet<_> = report
            .runtime
            .not_exact
            .values()
            .filter(|line| line.alias_results > 0)
            .filter_map(|line| line.alias_member.as_deref())
            .collect();
        assert_eq!(
            aliases,
            BTreeSet::from([
                "global/llamacpp-b1::linux-x64-rocm",
                "org_1/fork::linux-x64-rocm",
            ])
        );

        let text = report.render(3);
        assert!(text.contains("an alias would name"), "{text}");
        assert!(text.contains("no member has this build"), "{text}");
        Ok(())
    }
}
