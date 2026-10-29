//! A plan's results, read from pipette-duckdb, which publishes mgmt's
//! `plan_jobs` beside the warehouse so one query joins them. Both republish on
//! its refresh tick, so a result or plan filed since is missing until then.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};

use pipette_core::canonical_json::sort_keys;
use pipette_core::config::DuckDbConfig;
use pipette_core::warehouse::SYSTEM_CLIENT_ID;
use pipette_plan_types::{
    BenchmarkFlags, BenchmarkType, DeviceTimeTotal, ElapsedMillis, FailureSource, FleetDetail,
    JobId, Model, ModelFlags, ModelType, OrUnknown, PlanId, Runtime, RuntimeFlags, RuntimeType,
    UnknownValue,
};

use crate::model_params::ModelParamTable;

/// The `device_*` columns a result row carries. Append new columns, so the
/// device block keeps its order.
pub const DEVICE_COLUMNS: &[&str] = &[
    "device_form_factor",
    "device_os_name",
    "device_os_version",
    "device_chip_model",
    "device_ram_bytes",
    "device_battery_level",
    "device_power_state",
    "device_power_save_mode",
    "device_apple_thermal_state_before",
    "device_apple_thermal_state_after",
    "device_apple_soc_temp_c_before",
    "device_apple_soc_temp_c_after",
    "device_android_thermal_status_before",
    "device_android_thermal_status_after",
    "device_android_thermal_headroom_before",
    "device_android_thermal_headroom_after",
    "device_gpu_model",
    "device_gpu_vram_bytes",
    "device_npu_model",
    "device_npu_vram_bytes",
    "device_os_build",
    "device_os_security_patch",
    "device_cpu_arch",
    "device_os_family",
];

/// One metric of one job's latest scoring run.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ResultRow {
    pub job_id: JobId,
    pub benchmark_id: String,
    pub benchmark_type: String,
    pub metric: String,
    /// `null` on a failure row.
    pub value: Option<f32>,
    pub value_stddev: Option<f32>,
    pub unit: String,
    /// `null` on a row written before the column existed, which was no failure.
    pub failed: Option<bool>,
    pub failed_reason: Option<String>,
    /// The descriptors the client reported, as the warehouse stores them.
    pub model_descriptor: Option<String>,
    /// The digest of `model_descriptor` that the scorer stored.
    pub model_descriptor_sha256: Option<String>,
    pub runtime_descriptor: Option<String>,
    /// The digest of `runtime_descriptor` that the scorer stored, without a
    /// vault `custom_label`. `null` on a row written before the column existed.
    pub runtime_descriptor_sha256: Option<String>,
    pub model_vault_descriptor: Option<String>,
    /// The flag groups the run executed with, as its client reported them.
    pub model_flags: Option<String>,
    pub runtime_flags: Option<String>,
    pub benchmark_flags: Option<String>,
    /// The flag groups the job asked for, canonicalized.
    pub requested_model_flags: Option<String>,
    pub requested_runtime_flags: Option<String>,
    pub requested_benchmark_flags: Option<String>,
    pub eval_metadata: Option<String>,
    pub client_extras: Option<String>,
    /// The row's non-null `parameter_*` columns, without the prefix.
    pub parameters: BTreeMap<String, Value>,
    /// The row's non-null `observation_*` columns, without the prefix: what the
    /// run measured beside its metrics, such as its peak swap.
    pub observations: BTreeMap<String, Value>,
    /// As the warehouse has it: [`SYSTEM_CLIENT_ID`] on a failure the server
    /// declared, which the body reads as `null`.
    pub client_id: Option<String>,
    pub device_name: Option<String>,
    /// The non-null `device_*` columns of [`DEVICE_COLUMNS`], without the prefix.
    pub device: BTreeMap<String, Value>,
    pub client_version: Option<String>,
    pub submitted_at: Option<DateTime<Utc>>,
    pub scored_at: Option<DateTime<Utc>>,
    /// Device milliseconds on the job as the client reported them; every row of
    /// one scoring run carries the same value.
    pub device_time_ms: Option<i64>,
    /// Which pass declared the failure; `null` on a measured row, an older or
    /// unknown source, and below `read_clients` for any but `expired` and `lease_lost`.
    #[serde(default, deserialize_with = "known_source")]
    pub failure_source: Option<FailureSource>,
    /// The job whose recorded completions a re-scored copy scores again.
    #[serde(default, deserialize_with = "valid_job_id")]
    pub rescored_from: Option<JobId>,
    /// The time that a device submitted the run that the row scores. It equals
    /// `submitted_at`, except on a re-scored copy. There it is the time of the
    /// run that the copy scores again. `null` where the warehouse holds none.
    pub run_submitted_at: Option<DateTime<Utc>>,
    /// The `runtime_version` that the scoring service returned for an eval
    /// row. It is `null` on every other row and on a failed eval row.
    pub score_runtime_version: Option<String>,
    /// Parameter counts in millions, as the table had them at scoring or
    /// `fix-model-param`; `null` while the table has no entry.
    pub model_params_total_millions: Option<i32>,
    pub model_params_active_millions: Option<i32>,
}

impl From<ResultRow> for pipette_plan_types::ResultRow {
    fn from(row: ResultRow) -> Self {
        // Destructured, so a column added to the row is a decision here.
        let ResultRow {
            job_id,
            benchmark_id,
            benchmark_type,
            metric,
            value,
            value_stddev,
            unit,
            failed,
            failed_reason,
            model_descriptor,
            model_descriptor_sha256,
            runtime_descriptor,
            runtime_descriptor_sha256,
            model_vault_descriptor,
            model_flags,
            runtime_flags,
            benchmark_flags,
            requested_model_flags,
            requested_runtime_flags,
            requested_benchmark_flags,
            eval_metadata,
            client_extras,
            parameters,
            observations,
            client_id,
            device_name,
            device,
            client_version,
            submitted_at,
            scored_at,
            device_time_ms,
            failure_source: source,
            rescored_from,
            run_submitted_at,
            score_runtime_version,
            model_params_total_millions,
            model_params_active_millions,
        } = row;
        let model_descriptor: Option<OrUnknown<Model>> = model_descriptor.as_deref().map(decoded);
        let runtime_descriptor: Option<OrUnknown<Runtime>> =
            runtime_descriptor.as_deref().map(decoded);
        // Recorded flags carry no cell; read them back against the row's.
        let model = known(&model_descriptor).map(ModelType::of);
        let runtime = known(&runtime_descriptor).map(RuntimeType::of);
        let benchmark = benchmark_type.parse::<BenchmarkType>().ok();
        let model_flags = model_flags.and_then(|stored| match model.zip(benchmark) {
            Some((model, benchmark)) => {
                match ModelFlags::from_canonical_string(&stored, benchmark, model) {
                    Some(flags) => Some(OrUnknown::Known(flags)),
                    None => recorded(
                        ModelFlags::from_cell_json(&stored, model, benchmark),
                        &stored,
                    ),
                }
            }
            None => Some(stored_unknown(
                &stored,
                "no model or benchmark names the cell",
            )),
        });
        let cell = runtime.zip(model).zip(benchmark);
        let runtime_flags =
            runtime_flags.and_then(|stored| in_cell(&stored, cell, RuntimeFlags::from_cell_json));
        let benchmark_flags = benchmark_flags
            .and_then(|stored| in_cell(&stored, cell, BenchmarkFlags::from_cell_json));
        pipette_plan_types::ResultRow {
            job_id,
            benchmark_id,
            benchmark_type,
            metric,
            value,
            value_stddev,
            unit,
            failed,
            failed_reason,
            model_descriptor,
            model_descriptor_sha256,
            runtime_descriptor,
            runtime_descriptor_sha256,
            model_vault_descriptor: model_vault_descriptor.as_deref().map(decoded),
            model_flags,
            runtime_flags,
            benchmark_flags,
            requested_model_flags: requested_model_flags.as_deref().map(decoded),
            requested_runtime_flags: requested_runtime_flags.as_deref().map(decoded),
            requested_benchmark_flags: requested_benchmark_flags.as_deref().map(decoded),
            eval_metadata: eval_metadata.as_deref().map(decoded),
            client_extras: client_extras.as_deref().map(decoded),
            parameters,
            observations,
            // The reserved id names no machine, so a reader must not group it as one.
            client_id: client_id.filter(|id| id != SYSTEM_CLIENT_ID),
            device_name,
            device,
            client_version,
            submitted_at,
            scored_at,
            // Scoring writes it from an `ElapsedMillis`; anything else is no reading.
            device_time_ms: device_time_ms.and_then(|ms| ElapsedMillis::try_new(ms).ok()),
            failure_source: source,
            rescored_from,
            run_submitted_at,
            score_runtime_version,
            model_params_total_millions,
            model_params_active_millions,
        }
    }
}

/// A stored JSON column as `T`, or as stored where it does not decode.
pub fn decoded<T: DeserializeOwned>(stored: &str) -> OrUnknown<T> {
    value_decoded(stored_json(stored))
}

/// `value` as `T`, or wrapped as unknown.
pub fn value_decoded<T: DeserializeOwned>(value: Value) -> OrUnknown<T> {
    match T::deserialize(&value) {
        Ok(known) => OrUnknown::Known(known),
        Err(error) => unknown(value, &error),
    }
}

/// Flags recorded for a `(runtime, model, benchmark)` cell, read back against
/// the row's, or as stored where the row names none.
fn in_cell<T, E: std::fmt::Display>(
    stored: &str,
    cell: Option<((RuntimeType, ModelType), BenchmarkType)>,
    read: impl FnOnce(&str, RuntimeType, ModelType, BenchmarkType) -> Result<Option<T>, E>,
) -> Option<OrUnknown<T>> {
    match cell {
        Some(((runtime, model), benchmark)) => {
            recorded(read(stored, runtime, model, benchmark), stored)
        }
        None => Some(stored_unknown(
            stored,
            "no runtime, model or benchmark names the cell",
        )),
    }
}

fn known<T>(decoded: &Option<OrUnknown<T>>) -> Option<&T> {
    match decoded {
        Some(OrUnknown::Known(value)) => Some(value),
        _ => None,
    }
}

/// Flags read back against their cell: known, `None` for an empty object, or
/// as stored where the cell refuses them.
fn recorded<T, E: std::fmt::Display>(
    read: Result<Option<T>, E>,
    stored: &str,
) -> Option<OrUnknown<T>> {
    match read {
        Ok(flags) => flags.map(OrUnknown::Known),
        Err(error) => Some(stored_unknown(stored, error)),
    }
}

fn stored_unknown<T>(stored: &str, error: impl std::fmt::Display) -> OrUnknown<T> {
    unknown(stored_json(stored), &error)
}

fn unknown<T>(value: Value, error: &dyn std::fmt::Display) -> OrUnknown<T> {
    tracing::debug!(%error, "a stored value does not decode; it reads as unknown");
    OrUnknown::Unknown(UnknownValue::new(sort_keys(value)))
}

/// Stored text as JSON, or as a JSON string where it is not JSON.
fn stored_json(stored: &str) -> Value {
    serde_json::from_str(stored).unwrap_or_else(|_| Value::String(stored.to_string()))
}

impl ResultRow {
    /// Fill counts the warehouse lacks from the table, matched as the scorer
    /// matches. A stored count is kept even where the table now disagrees.
    pub fn fill_model_params(&mut self, table: &ModelParamTable) {
        if self.model_params_total_millions.is_some() {
            return;
        }
        if let Some((_, entry)) = table.resolve_recorded(
            self.model_descriptor.as_deref(),
            self.model_vault_descriptor.as_deref(),
        ) {
            self.model_params_total_millions = Some(entry.total);
            self.model_params_active_millions = Some(entry.active);
        }
    }

    /// Drop what names a fleet member, for a reader below `read_clients`. The
    /// failure reason is free text that can list client ids, so it goes too.
    pub fn withhold_fleet(&mut self) {
        self.client_id = None;
        self.device_name = None;
        self.device.clear();
        self.client_version = None;
        // Free-form, so it can name the machine.
        self.client_extras = None;
        self.failed_reason = None;
        // Keep only what describes the job itself, so a null source also covers
        // `client` and does not single out a roster denial or a bad clause.
        self.failure_source = self
            .failure_source
            .filter(|source| matches!(source, FailureSource::Expired | FailureSource::LeaseLost));
    }
}

/// A source this build does not name reads as none: the scorer deploys apart from
/// the server, and a newer one's source must not fail the page.
fn known_source<'de, D: Deserializer<'de>>(source: D) -> Result<Option<FailureSource>, D::Error> {
    Ok(Option::<String>::deserialize(source)?.and_then(|source| source.parse().ok()))
}

/// A stored id outside the `JobId` charset reads as none: the earlier-runs
/// query pastes the id into its SQL literal.
fn valid_job_id<'de, D: Deserializer<'de>>(id: D) -> Result<Option<JobId>, D::Error> {
    Ok(Option::<String>::deserialize(id)?.and_then(|id| JobId::try_new(id).ok()))
}

/// What a result row selects, beside the job id, [`DEVICE_COLUMNS`] and the
/// `parameter_*` and `observation_*` columns.
/// `docs/storage.md` and `docs/api/plans/get_plan_results_csv.md` document it;
/// keep them in step.
const COLUMNS: &[&str] = &[
    "w.benchmark_id",
    "w.benchmark_type",
    "w.metric",
    "w.value",
    "w.value_stddev",
    "w.unit",
    "w.failed",
    "w.failed_reason",
    // Planned jobs report descriptors; `model_name` and `runtime_name` are empty.
    "w.model_descriptor",
    "w.model_descriptor_sha256",
    "w.runtime_descriptor",
    "w.runtime_descriptor_sha256",
    "w.model_vault_descriptor",
    "w.model_flags",
    "w.runtime_flags",
    "w.benchmark_flags",
    "w.requested_model_flags",
    "w.requested_runtime_flags",
    "w.requested_benchmark_flags",
    "w.eval_metadata",
    "w.client_extras",
    "w.client_id",
    "w.device_name",
    "w.submitted_at",
    "w.scored_at",
    "w.device_time_ms",
    "w.failure_source",
    "w.rescored_from",
    "w.run_submitted_at",
    "w.score_runtime_version",
    "w.client_version",
    "w.model_params_total_millions",
    "w.model_params_active_millions",
];

const PARAMETER_PREFIX: &str = "parameter_";
const OBSERVATION_PREFIX: &str = "observation_";
const DEVICE_PREFIX: &str = "device_";

/// `rows` by job, each job's rows in their order.
pub fn rows_by_job(rows: Vec<ResultRow>) -> HashMap<JobId, Vec<ResultRow>> {
    rows.into_iter().fold(HashMap::new(), |mut jobs, row| {
        jobs.entry(row.job_id.clone()).or_default().push(row);
        jobs
    })
}

/// A page of a plan's result rows.
#[derive(Debug)]
pub struct PlanResults {
    pub rows: Vec<ResultRow>,
    /// Result rows across every page.
    pub total: usize,
    /// The plan's jobs that have at least one row.
    pub jobs_reported: usize,
}

/// A page of a plan's results cut to a fleet tier; `plans results` prints the body it fills.
#[derive(Debug)]
pub struct ResultListView {
    pub fleet_detail: FleetDetail,
    pub plan_id: PlanId,
    /// Jobs the manifest lists.
    pub jobs: usize,
    /// Of those, the jobs with at least one result row.
    pub jobs_reported: usize,
    /// Result rows before paging.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    /// One row per metric of each job's latest scoring run, in the plan's job
    /// order, cut to the fleet tier.
    pub results: Vec<ResultRow>,
}

impl From<ResultListView> for pipette_plan_types::ResultListView {
    fn from(view: ResultListView) -> Self {
        Self {
            fleet_detail: view.fleet_detail,
            plan_id: view.plan_id,
            jobs: view.jobs,
            jobs_reported: view.jobs_reported,
            total: view.total,
            offset: view.offset,
            limit: view.limit,
            results: view.results.into_iter().map(Into::into).collect(),
        }
    }
}

impl ResultListView {
    /// The page read at `(offset, limit)`, with its rows cut to the `fleet` tier.
    pub fn new(
        fleet: FleetDetail,
        plan_id: PlanId,
        jobs: usize,
        mut page: PlanResults,
        offset: usize,
        limit: usize,
    ) -> Self {
        if let FleetDetail::Withheld = fleet {
            page.rows.iter_mut().for_each(ResultRow::withhold_fleet);
        }
        Self {
            fleet_detail: fleet,
            plan_id,
            jobs,
            jobs_reported: page.jobs_reported,
            total: page.total,
            offset,
            limit,
            results: page.rows,
        }
    }
}

/// `url::ParseError`, named through `reqwest` so this crate takes no `url` dependency.
type UrlParseError = <reqwest::Url as std::str::FromStr>::Err;

#[derive(Debug, thiserror::Error)]
pub enum ResultsError {
    #[error("[duckdb] url is invalid")]
    Url(#[source] UrlParseError),
    /// A window DuckDB's `BIGINT` `LIMIT`/`OFFSET` cannot hold: the caller's fault.
    #[error("offset {offset} or limit {limit} is out of range")]
    Window { offset: usize, limit: usize },
    #[error("pipette-duckdb unreachable")]
    Unreachable(#[source] reqwest::Error),
    #[error("pipette-duckdb answered {status}: {body}")]
    Status {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("pipette-duckdb answered an undecodable body")]
    Body(#[source] reqwest::Error),
    #[error("pipette-duckdb answered a result row mgmt cannot read")]
    Row(#[source] serde_json::Error),
    #[error("pipette-duckdb answered no counts row")]
    Counts,
    #[error("pipette-duckdb answered a device-time total mgmt cannot read")]
    DeviceTime,
}

impl ResultsError {
    /// Whether pipette-duckdb outlasted the read's timeout, as against failing it.
    pub fn is_timeout(&self) -> bool {
        match self {
            Self::Unreachable(e) | Self::Body(e) => e.is_timeout(),
            Self::Url(_)
            | Self::Window { .. }
            | Self::Status { .. }
            | Self::Row(_)
            | Self::Counts
            | Self::DeviceTime => false,
        }
    }
}

#[derive(Serialize)]
struct QueryBody<'a> {
    sql: &'a str,
}

/// The `/api/query` response: rows as arrays, in `columns` order.
#[derive(Deserialize)]
struct QueryResult {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

/// How long [`ResultsDb::job_results`] and [`ResultsDb::device_time_total`]
/// wait, whatever `[duckdb] timeout_secs` says; past it the page answers
/// without that data.
pub const JOB_RESULTS_TIMEOUT: Duration = Duration::from_secs(3);

/// The log target of each pipette-duckdb query, at `debug`, as `sqlx::query`
/// is sqlx's: `RUST_LOG=pipette::duckdb=debug` shows them.
pub const QUERY_LOG_TARGET: &str = "pipette::duckdb";

/// A pipette-duckdb client over [`DuckDbConfig`].
#[derive(Clone)]
pub struct ResultsDb {
    http: reqwest::Client,
    /// `{url}/api/query?db={database}`, parsed once so a bad `[duckdb] url`
    /// fails at startup rather than on every read.
    query_url: reqwest::Url,
    config: DuckDbConfig,
}

impl ResultsDb {
    pub fn new(http: reqwest::Client, config: DuckDbConfig) -> Result<Self, ResultsError> {
        let query_url = reqwest::Url::parse_with_params(
            &format!("{}/api/query", config.url.trim_end_matches('/')),
            [("db", &config.database)],
        )
        .map_err(ResultsError::Url)?;
        Ok(Self {
            http,
            query_url,
            config,
        })
    }

    /// The page `[offset, offset + limit)` of `plan_id`'s result rows, in the
    /// plan's job order, and the counts across every page.
    pub async fn plan_results(
        &self,
        plan_id: &PlanId,
        offset: usize,
        limit: usize,
    ) -> Result<PlanResults, ResultsError> {
        if i64::try_from(offset).is_err() || i64::try_from(limit).is_err() {
            return Err(ResultsError::Window { offset, limit });
        }
        let (page_sql, counts_sql) = (page_sql(plan_id, offset, limit), counts_sql(plan_id));
        let (page, counts) = futures::try_join!(
            self.query(&page_sql, self.timeout()),
            self.query(&counts_sql, self.timeout())
        )?;
        let (total, jobs_reported) = parse_counts(counts)?;
        Ok(PlanResults {
            rows: parse_rows(page)?,
            total,
            jobs_reported,
        })
    }

    /// Every result row of `plan_id`, in the plan's job order. No `LIMIT`:
    /// `plan_max_jobs` and `timeout_secs` bound it.
    pub async fn all_plan_results(&self, plan_id: &PlanId) -> Result<Vec<ResultRow>, ResultsError> {
        parse_rows(self.query(&all_sql(plan_id), self.timeout()).await?)
    }

    /// Every result row of `jobs`, each job at its latest scoring run, within
    /// [`JOB_RESULTS_TIMEOUT`].
    pub async fn job_results(&self, jobs: &[JobId]) -> Result<Vec<ResultRow>, ResultsError> {
        // An empty `VALUES` list is not SQL.
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        parse_rows(
            self.query(&jobs_sql(jobs), self.job_results_timeout())
                .await?,
        )
    }

    /// [`Self::job_results`] for a whole plan's jobs, within `timeout_secs`
    /// alone. `plan_max_jobs` bounds the list.
    pub async fn all_job_results(&self, jobs: &[JobId]) -> Result<Vec<ResultRow>, ResultsError> {
        if jobs.is_empty() {
            return Ok(Vec::new());
        }
        parse_rows(self.query(&jobs_sql(jobs), self.timeout()).await?)
    }

    /// The device time `plans` spent, a job listed by several counted once.
    /// `skip_rescored` drops re-scored copies, which repeat another job's run.
    pub async fn device_time_total(
        &self,
        plans: &[PlanId],
        skip_rescored: bool,
    ) -> Result<DeviceTimeTotal, ResultsError> {
        // An empty `IN` list is not SQL.
        if plans.is_empty() {
            return Ok(DeviceTimeTotal::default());
        }
        let timeout = self.timeout().min(JOB_RESULTS_TIMEOUT);
        parse_device_time(
            self.query(&device_time_sql(plans, skip_rescored), timeout)
                .await?,
        )
    }

    /// `[duckdb] timeout_secs`, which [`Self::all_job_results`] waits.
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.config.timeout_secs.get())
    }

    /// What [`Self::job_results`] waits.
    pub fn job_results_timeout(&self) -> Duration {
        self.timeout().min(JOB_RESULTS_TIMEOUT)
    }

    /// Every query is logged here with its duration, the statement cut to its
    /// head: a plan's `VALUES` list runs to tens of kilobytes.
    async fn query(&self, sql: &str, timeout: Duration) -> Result<QueryResult, ResultsError> {
        let started = std::time::Instant::now();
        let answer = self.send(sql, timeout).await;
        tracing::debug!(
            target: QUERY_LOG_TARGET,
            elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            rows = answer.as_ref().map_or(0, |result| result.rows.len()),
            ok = answer.is_ok(),
            sql_len = sql.len(),
            sql = %sql.chars().take(300).collect::<String>(),
            "pipette-duckdb query"
        );
        answer
    }

    async fn send(&self, sql: &str, timeout: Duration) -> Result<QueryResult, ResultsError> {
        let resp = self
            .http
            .post(self.query_url.clone())
            .basic_auth(&self.config.user, Some(self.config.password.expose()))
            .timeout(timeout)
            .json(&QueryBody { sql })
            .send()
            .await
            .map_err(ResultsError::Unreachable)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ResultsError::Status {
                status,
                body: body.chars().take(500).collect(),
            });
        }
        resp.json().await.map_err(ResultsError::Body)
    }
}

/// `plan_id` is `[A-Za-z0-9-]` by construction, so it goes into the literal as is.
fn plan_jobs(plan_id: &PlanId) -> String {
    format!("SELECT job_id, ordinal FROM plan_jobs WHERE plan_id = '{plan_id}'")
}

/// The join every query shares: `jobs` at their latest scoring run. A re-score
/// appends rows rather than replacing them, so the newest `scored_at` wins.
fn joined(jobs: &str, select: &str) -> String {
    format!(
        "SELECT {select} FROM ({jobs}) pj \
         JOIN warehouse w ON w.submission_id = pj.job_id \
         QUALIFY w.scored_at = max(w.scored_at) OVER (PARTITION BY pj.job_id)"
    )
}

fn rows_sql(jobs: &str) -> String {
    let columns: String = COLUMNS
        .iter()
        .map(|c| format!(", {c}"))
        .chain(DEVICE_COLUMNS.iter().map(|c| format!(", w.{c}")))
        .collect();
    // `result_id` breaks the tie, so a page boundary is stable across requests.
    format!(
        "{} ORDER BY pj.ordinal, pj.job_id, w.metric, w.result_id",
        joined(
            jobs,
            &format!(
                "pj.job_id{columns}, COLUMNS('^{PARAMETER_PREFIX}'), \
                 COLUMNS('^{OBSERVATION_PREFIX}')"
            )
        )
    )
}

fn page_sql(plan_id: &PlanId, offset: usize, limit: usize) -> String {
    format!("{} LIMIT {limit} OFFSET {offset}", all_sql(plan_id))
}

fn all_sql(plan_id: &PlanId) -> String {
    rows_sql(&plan_jobs(plan_id))
}

/// By id, not through `plan_jobs`, which lags a plan filed since the last refresh.
/// A `JobId` is `[A-Za-z0-9-]` by construction, so it goes into the literal as is.
fn jobs_sql(jobs: &[JobId]) -> String {
    let values = jobs
        .iter()
        .enumerate()
        .map(|(ordinal, id)| format!("('{id}', {ordinal})"))
        .collect::<Vec<_>>()
        .join(", ");
    rows_sql(&format!(
        "SELECT * FROM (VALUES {values}) v(job_id, ordinal)"
    ))
}

fn counts_sql(plan_id: &PlanId) -> String {
    format!(
        "SELECT count(*) AS total, count(DISTINCT job_id) AS jobs FROM ({})",
        joined(&plan_jobs(plan_id), "pj.job_id")
    )
}

/// `any_value` takes a job's first non-null value, as one run's rows share it.
/// `unreported_job_count` leaves out server-declared failures, which ran on no
/// device; one stored before `failure_source` is marked by [`SYSTEM_CLIENT_ID`].
fn device_time_sql(plans: &[PlanId], skip_rescored: bool) -> String {
    // A `PlanId` is `[A-Za-z0-9-]` by construction, so the literal needs no escaping.
    let plans = plans
        .iter()
        .map(|plan_id| format!("'{plan_id}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let jobs = format!("SELECT DISTINCT job_id FROM plan_jobs WHERE plan_id IN ({plans})");
    // A job's rows share one `rescored_from`, so the filter keeps or drops the job
    // whole; the source job still counts the device run once.
    let filter = if skip_rescored {
        " WHERE rescored_from IS NULL"
    } else {
        ""
    };
    // `sum` over `BIGINT` is a `HUGEINT`, which JSON would not carry as a number,
    // so cap at `i64::MAX` where a `CAST` would fail; `CASE` keeps `NULL` `NULL`.
    let (client, system, max) = (FailureSource::Client, SYSTEM_CLIENT_ID, i64::MAX);
    format!(
        "SELECT CAST(CASE WHEN sum(device_time_ms) > {max} THEN {max} \
         ELSE sum(device_time_ms) END AS BIGINT) AS device_time_ms, \
         count(*) FILTER (WHERE device_time_ms IS NULL \
         AND (failure_source IS NULL OR failure_source = '{client}') \
         AND client_id <> '{system}') AS unreported_job_count \
         FROM (SELECT job_id, any_value(device_time_ms) AS device_time_ms, \
         any_value(failure_source) AS failure_source, any_value(client_id) AS client_id \
         FROM ({}){filter} GROUP BY job_id)",
        joined(
            &jobs,
            "pj.job_id, w.device_time_ms, w.rescored_from, w.failure_source, w.client_id"
        )
    )
}

fn parse_rows(result: QueryResult) -> Result<Vec<ResultRow>, ResultsError> {
    result
        .rows
        .into_iter()
        .map(|values| {
            // Each prefixed family folds into its own map, its null columns dropped.
            let folded = [
                (PARAMETER_PREFIX, "parameters", Map::new()),
                (OBSERVATION_PREFIX, "observations", Map::new()),
                (DEVICE_PREFIX, "device", Map::new()),
            ];
            let (mut row, folded) = result.columns.iter().zip(values).fold(
                (Map::new(), folded),
                |(mut row, mut folded), (column, value)| {
                    let family = folded.iter_mut().find_map(|(prefix, _, map)| {
                        column
                            .strip_prefix(*prefix)
                            // `device_name` is a field of its own, not one of the device's.
                            .filter(|_| {
                                *prefix != DEVICE_PREFIX
                                    || DEVICE_COLUMNS.contains(&column.as_str())
                            })
                            .map(|name| (name, map))
                    });
                    match family {
                        Some(_) if value.is_null() => {}
                        Some((name, map)) => {
                            map.insert(name.to_string(), value);
                        }
                        None => {
                            row.insert(column.clone(), value);
                        }
                    }
                    (row, folded)
                },
            );
            row.extend(
                folded
                    .into_iter()
                    .map(|(_, field, map)| (field.to_string(), Value::Object(map))),
            );
            serde_json::from_value(Value::Object(row)).map_err(ResultsError::Row)
        })
        .collect()
}

fn parse_counts(result: QueryResult) -> Result<(usize, usize), ResultsError> {
    let count = |i: usize| {
        result
            .rows
            .first()
            .and_then(|row| row.get(i))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or(ResultsError::Counts)
    };
    Ok((count(0)?, count(1)?))
}

fn parse_device_time(result: QueryResult) -> Result<DeviceTimeTotal, ResultsError> {
    let cell = |i: usize| {
        result
            .rows
            .first()
            .and_then(|row| row.get(i))
            .ok_or(ResultsError::DeviceTime)
    };
    let sum = cell(0)?;
    let device_time_ms = (!sum.is_null())
        .then(|| {
            sum.as_i64()
                .and_then(|ms| ElapsedMillis::try_new(ms).ok())
                .ok_or(ResultsError::DeviceTime)
        })
        .transpose()?;
    let unreported_job_count = cell(1)?
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(ResultsError::DeviceTime)?;
    Ok(DeviceTimeTotal {
        device_time_ms,
        unreported_job_count,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use pipette_plan_types::FailureSource;

    use super::*;

    fn plan() -> anyhow::Result<PlanId> {
        Ok(PlanId::try_new(
            "plan-0192b3c4-0000-7000-8000-000000000001",
        )?)
    }

    /// A misspelt or renamed column would fail every results read at pipette-duckdb.
    #[test]
    fn every_device_column_is_a_warehouse_column() {
        let schema = pipette_core::warehouse::parquet_schema();
        let missing: Vec<_> = DEVICE_COLUMNS
            .iter()
            .filter(|column| schema.field_with_name(column).is_err())
            .collect();
        assert!(missing.is_empty(), "not warehouse columns: {missing:?}");
    }

    #[test]
    fn the_page_query_names_the_plan_and_windows_the_rows() -> anyhow::Result<()> {
        let sql = page_sql(&plan()?, 200, 100);
        assert!(
            sql.contains("WHERE plan_id = 'plan-0192b3c4-0000-7000-8000-000000000001') pj"),
            "{sql}"
        );
        assert!(sql.contains("COLUMNS('^parameter_')"));
        assert!(sql.contains("COLUMNS('^observation_')"), "{sql}");
        assert!(
            sql.contains(", w.device_apple_thermal_state_after"),
            "{sql}"
        );
        assert!(sql.contains(", w.client_version"), "{sql}");
        assert!(sql.contains(", w.score_runtime_version"), "{sql}");
        assert!(sql.ends_with("LIMIT 100 OFFSET 200"), "{sql}");
        assert!(!sql.contains(';'));
        Ok(())
    }

    #[test]
    fn the_whole_query_names_the_plan_and_reads_every_row() -> anyhow::Result<()> {
        let sql = all_sql(&plan()?);
        assert!(
            sql.contains("WHERE plan_id = 'plan-0192b3c4-0000-7000-8000-000000000001') pj"),
            "{sql}"
        );
        assert!(sql.contains("COLUMNS('^parameter_')"));
        assert!(
            sql.ends_with("ORDER BY pj.ordinal, pj.job_id, w.metric, w.result_id"),
            "{sql}"
        );
        assert!(!sql.contains("LIMIT") && !sql.contains(';'), "{sql}");
        Ok(())
    }

    #[test]
    fn the_jobs_query_names_each_job_in_order_and_not_the_plan() -> anyhow::Result<()> {
        let sql = jobs_sql(&[JobId::try_new("job-b")?, JobId::try_new("job-a")?]);
        assert!(
            sql.contains(
                "FROM (SELECT * FROM (VALUES ('job-b', 0), ('job-a', 1)) v(job_id, ordinal)) pj"
            ),
            "{sql}"
        );
        assert!(sql.contains("COLUMNS('^parameter_')"));
        assert!(
            !sql.contains("plan_jobs") && !sql.contains("LIMIT"),
            "{sql}"
        );
        assert!(!sql.contains(';'));
        Ok(())
    }

    /// A failed row of `job-1` whose `column` cell is the JSON `cell`.
    fn row_with(column: &str, cell: &str) -> anyhow::Result<ResultRow> {
        let result: QueryResult = serde_json::from_str(&format!(
            r#"{{
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "failed", "failed_reason",
                            "model_descriptor", "runtime_descriptor", "client_id", "device_name",
                            "submitted_at", "scored_at", "device_time_ms", "{column}"],
                "rows": [["job-1", "llama-bench", "perf", "tokens_per_second", null, null,
                          "tok/s", true, "1 client (ev1_a): out of memory", null, null, "ev1_a",
                          "Pixel 9", null, "2026-09-29T10:05:00.000000Z", 9500, {cell}]]
            }}"#
        ))?;
        parse_rows(result)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no row"))
    }

    fn failed_row(source: &str) -> anyhow::Result<ResultRow> {
        row_with("failure_source", source)
    }

    #[test]
    fn withholding_the_fleet_drops_the_device_its_state_and_the_failure_reason()
    -> anyhow::Result<()> {
        let mut row = failed_row("null")?;
        row.device
            .insert("power_state".to_string(), Value::from("charging"));
        row.client_version = Some("1.4.0".to_string());
        row.client_extras = Some(r#"{"host":"bench-a"}"#.to_string());
        row.score_runtime_version = Some("1.2.3".to_string());
        row.observations
            .insert("max_swap_bytes".to_string(), Value::from(0));
        row.withhold_fleet();
        assert_eq!(row.failed, Some(true));
        assert_eq!(
            (&row.failed_reason, &row.client_id, &row.device_name),
            (&None, &None, &None)
        );
        assert!(row.device.is_empty());
        assert_eq!((&row.client_version, &row.client_extras), (&None, &None));
        // What the run measured is the job's, not the fleet's.
        assert_eq!(row.observations.len(), 1);
        // The device time and the scorer version name no fleet member.
        assert_eq!(row.device_time_ms, Some(9500));
        assert_eq!(row.score_runtime_version.as_deref(), Some("1.2.3"));
        Ok(())
    }

    /// The record keeps the warehouse's reserved id; the body names no machine.
    #[test]
    fn a_failure_the_server_declared_names_no_machine_in_the_body() -> anyhow::Result<()> {
        let mut row = failed_row(r#""lease_lost""#)?;
        row.client_id = Some(SYSTEM_CLIENT_ID.to_string());
        row.device_name = None;
        let body = pipette_plan_types::ResultRow::from(row.clone());
        assert_eq!(row.client_id.as_deref(), Some(SYSTEM_CLIENT_ID));
        assert_eq!(body.client_id, None);
        assert_eq!(body.failure_source, Some(FailureSource::LeaseLost));
        // A device's id passes through.
        let device = pipette_plan_types::ResultRow::from(failed_row("null")?);
        assert_eq!(device.client_id.as_deref(), Some("ev1_a"));
        Ok(())
    }

    #[rstest::rstest]
    #[case::client(r#""client""#, Some(FailureSource::Client), None)]
    #[case::expired(
        r#""expired""#,
        Some(FailureSource::Expired),
        Some(FailureSource::Expired)
    )]
    #[case::lease_lost(
        r#""lease_lost""#,
        Some(FailureSource::LeaseLost),
        Some(FailureSource::LeaseLost)
    )]
    #[case::all_clients_denied(
        r#""all_clients_denied""#,
        Some(FailureSource::AllClientsDenied),
        None
    )]
    #[case::unmatchable(r#""unmatchable""#, Some(FailureSource::Unmatchable), None)]
    #[case::unnamed(r#""rebooted""#, None, None)]
    #[case::null("null", None, None)]
    fn a_failure_source_parses_and_withholds(
        #[case] cell: &str,
        #[case] parsed: Option<FailureSource>,
        #[case] withheld: Option<FailureSource>,
    ) -> anyhow::Result<()> {
        let mut row = failed_row(cell)?;
        assert_eq!(row.failure_source, parsed);
        row.withhold_fleet();
        assert_eq!(row.failure_source, withheld);
        Ok(())
    }

    #[rstest::rstest]
    #[case::job_id(r#""job-0""#, Some("job-0"))]
    #[case::quote(r#""job-0' OR 1=1""#, None)]
    #[case::dot(r#""job.0""#, None)]
    #[case::null("null", None)]
    fn a_rescored_from_outside_the_job_id_charset_reads_as_none(
        #[case] cell: &str,
        #[case] parsed: Option<&str>,
    ) -> anyhow::Result<()> {
        assert_eq!(
            row_with("rescored_from", cell)?
                .rescored_from
                .as_ref()
                .map(JobId::as_str),
            parsed
        );
        Ok(())
    }

    #[test]
    fn a_row_folds_its_parameters_observations_and_device_and_drops_the_null_ones()
    -> anyhow::Result<()> {
        let result: QueryResult = serde_json::from_str(
            r#"{
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "failed", "failed_reason",
                            "model_descriptor", "runtime_descriptor", "client_id", "device_name",
                            "submitted_at", "scored_at",
                            "client_version", "score_runtime_version", "device_power_state",
                            "device_apple_thermal_state_after", "device_battery_level",
                            "parameter_prefill_tokens", "parameter_eval_id",
                            "observation_max_swap_bytes", "observation_output_digest"],
                "rows": [["job-1", "llama-bench", "perf", "tokens_per_second", 12.5, null,
                          "tok/s", null, null, "{\"path\":\"m.gguf\"}", null, "c1", "Pixel 9",
                          "2026-09-29T10:00:00.000000Z", "2026-09-29T10:05:00.000000Z",
                          "1.4.0", "1.2.3", "charging", ["nominal", "fair"], null, 512, null,
                          1048576, null]]
            }"#,
        )?;
        let rows = parse_rows(result)?;
        assert_eq!(rows.len(), 1);
        let row = rows.first().ok_or_else(|| anyhow::anyhow!("no row"))?;
        assert_eq!(row.job_id.as_str(), "job-1");
        assert_eq!(row.value, Some(12.5));
        assert_eq!(row.failed, None);
        assert_eq!(
            row.model_descriptor.as_deref(),
            Some(r#"{"path":"m.gguf"}"#)
        );
        // A column the query did not name reads as absent.
        assert_eq!((&row.runtime_flags, row.device_time_ms), (&None, None));
        assert_eq!(
            row.parameters,
            BTreeMap::from([("prefill_tokens".to_string(), Value::from(512))])
        );
        assert_eq!(
            row.observations,
            BTreeMap::from([("max_swap_bytes".to_string(), Value::from(1_048_576))])
        );
        assert_eq!(
            row.device,
            BTreeMap::from([
                (
                    "apple_thermal_state_after".to_string(),
                    serde_json::json!(["nominal", "fair"])
                ),
                ("power_state".to_string(), Value::from("charging")),
            ])
        );
        assert_eq!(row.device_name.as_deref(), Some("Pixel 9"));
        assert_eq!(row.client_version.as_deref(), Some("1.4.0"));
        assert_eq!(row.score_runtime_version.as_deref(), Some("1.2.3"));
        assert_eq!(
            row.scored_at.map(|at| at.to_rfc3339()),
            Some("2026-09-29T10:05:00+00:00".to_string())
        );
        Ok(())
    }

    #[test]
    fn a_row_reads_the_flags_run_and_the_flags_asked_for() -> anyhow::Result<()> {
        let result: QueryResult = serde_json::from_str(
            r#"{
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit", "model_flags", "runtime_flags",
                            "benchmark_flags", "requested_model_flags",
                            "requested_runtime_flags", "requested_benchmark_flags"],
                "rows": [["job-1", "llama-bench", "perf", "tokens_per_second", 12.5, null,
                          "tok/s", null, "{\"threads\":8}", null, null,
                          "{\"runtime_type\":\"llama_cpp\",\"threads\":8}", "{\"repetitions\":3}"]]
            }"#,
        )?;
        let row = parse_rows(result)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no row"))?;
        assert_eq!(
            (
                row.model_flags.as_deref(),
                row.runtime_flags.as_deref(),
                row.benchmark_flags.as_deref(),
            ),
            (None, Some(r#"{"threads":8}"#), None)
        );
        assert_eq!(
            (
                row.requested_model_flags.as_deref(),
                row.requested_runtime_flags.as_deref(),
                row.requested_benchmark_flags.as_deref(),
            ),
            (
                None,
                Some(r#"{"runtime_type":"llama_cpp","threads":8}"#),
                Some(r#"{"repetitions":3}"#)
            )
        );
        Ok(())
    }

    const GGUF: &str = r#"{"org":"meta-llama","path":"Q4_K_M.gguf","repo_name":"llama-3.2-1b","source":"huggingface","type":"gguf_text"}"#;
    const LLAMACPP: &str = r#"{"flavor":"macos-arm64","repository_url":"github.com/ggml-org/llama.cpp","repository_version":"b5000","source":"github_release","type":"llamacpp_cli_stock_tools"}"#;

    /// The body of a row of `benchmark_type` with `columns` set to stored texts.
    fn body_of(benchmark_type: &str, columns: &[(&str, &str)]) -> anyhow::Result<Value> {
        let mut names = vec![
            "job_id",
            "benchmark_id",
            "benchmark_type",
            "metric",
            "value",
            "value_stddev",
            "unit",
        ];
        names.extend(columns.iter().map(|(name, _)| *name));
        let mut cells = vec![
            Value::from("job-1"),
            Value::from("bench"),
            Value::from(benchmark_type),
            Value::from("m"),
            Value::from(1.0),
            Value::Null,
            Value::from("ms"),
        ];
        cells.extend(columns.iter().map(|(_, stored)| Value::from(*stored)));
        let result = QueryResult {
            columns: names.into_iter().map(str::to_string).collect(),
            rows: vec![cells],
        };
        let row = parse_rows(result)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no row"))?;
        Ok(serde_json::to_value(pipette_plan_types::ResultRow::from(
            row,
        ))?)
    }

    /// A known value is the plan-types value, which a reported flag group
    /// takes its axes for from the row.
    #[test]
    fn a_row_body_decodes_each_json_column() -> anyhow::Result<()> {
        let body = body_of(
            "prefill_throughput",
            &[
                ("model_descriptor", GGUF),
                ("runtime_descriptor", LLAMACPP),
                ("model_vault_descriptor", GGUF),
                ("runtime_flags", r#"{"threads":4}"#),
                (
                    "requested_runtime_flags",
                    r#"{"benchmark_type":"prefill_throughput","model_type":"gguf_text","runtime_type":"llamacpp_cli_stock_tools","threads":8}"#,
                ),
                ("eval_metadata", r#"{"samples_failed":3}"#),
                ("client_extras", r#"{"peak_rss_kib":10}"#),
            ],
        )?;
        for column in ["model_descriptor", "model_vault_descriptor"] {
            assert_eq!(
                body.get(column),
                Some(&serde_json::from_str(GGUF)?),
                "{column}"
            );
        }
        assert_eq!(
            body.get("runtime_descriptor"),
            Some(&serde_json::from_str(LLAMACPP)?)
        );
        let cell = serde_json::json!({
            "benchmark_type": "prefill_throughput",
            "model_type": "gguf_text",
            "runtime_type": "llamacpp_cli_stock_tools",
        });
        let with = |threads: u32| -> anyhow::Result<Value> {
            let mut flags = cell.clone();
            flags
                .as_object_mut()
                .ok_or_else(|| anyhow::anyhow!("an object"))?
                .insert("threads".to_string(), threads.into());
            Ok(flags)
        };
        assert_eq!(body.get("runtime_flags"), Some(&with(4)?));
        assert_eq!(body.get("requested_runtime_flags"), Some(&with(8)?));
        assert_eq!(
            body.get("eval_metadata"),
            Some(&serde_json::json!({"samples_failed": 3}))
        );
        assert_eq!(
            body.get("client_extras"),
            Some(&serde_json::json!({"peak_rss_kib": 10}))
        );
        assert_eq!(body.get("model_flags"), Some(&Value::Null));
        Ok(())
    }

    /// Model flags take the model's type and the benchmark's, not the runtime's;
    /// benchmark flags take all three.
    #[test]
    fn reported_flags_take_the_axes_their_type_does() -> anyhow::Result<()> {
        let body = body_of(
            "eval",
            &[
                ("model_descriptor", GGUF),
                ("runtime_descriptor", LLAMACPP),
                ("model_flags", r#"{"enable_thinking":true}"#),
                ("benchmark_flags", r#"{"http_timeout_seconds":600}"#),
            ],
        )?;
        assert_eq!(
            body.get("model_flags"),
            Some(&serde_json::json!({
                "benchmark_type": "eval",
                "model_type": "gguf_text",
                "enable_thinking": true,
            }))
        );
        assert_eq!(
            body.get("benchmark_flags"),
            Some(&serde_json::json!({
                "benchmark_type": "eval",
                "model_type": "gguf_text",
                "runtime_type": "llamacpp_cli_stock_tools",
                "http_timeout_seconds": 600,
            }))
        );
        Ok(())
    }

    /// The body carries the typed value as Rust writes it, where the record (and
    /// so the CSV) keeps the stored text: a `null` optional field drops out.
    #[test]
    fn the_body_types_a_descriptor_the_record_keeps_as_stored() -> anyhow::Result<()> {
        let stored = r#"{"org":"meta-llama","path":"Q4_K_M.gguf","repo_name":"llama-3.2-1b","revision":null,"source":"huggingface","type":"gguf_text"}"#;
        let body = body_of("eval", &[("model_descriptor", stored)])?;
        assert_eq!(
            body.get("model_descriptor"),
            Some(&serde_json::from_str(GGUF)?)
        );
        let result = QueryResult {
            columns: [
                "job_id",
                "benchmark_id",
                "benchmark_type",
                "metric",
                "value",
                "value_stddev",
                "unit",
                "model_descriptor",
            ]
            .map(str::to_string)
            .to_vec(),
            rows: vec![vec![
                Value::from("job-1"),
                Value::from("bench"),
                Value::from("eval"),
                Value::from("m"),
                Value::from(1.0),
                Value::Null,
                Value::from("ms"),
                Value::from(stored),
            ]],
        };
        let record = parse_rows(result)?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("no row"))?;
        assert_eq!(record.model_descriptor.as_deref(), Some(stored));
        Ok(())
    }

    /// Clients report an eval's model flags as `enable_thinking=<bool>`, the
    /// spelling plan-types writes, so it reads back as those flags.
    #[rstest::rstest]
    #[case::thinking_on("enable_thinking=true", true)]
    #[case::thinking_off("enable_thinking=false", false)]
    fn a_reported_canonical_string_reads_as_model_flags(
        #[case] stored: &str,
        #[case] thinking: bool,
    ) -> anyhow::Result<()> {
        let body = body_of(
            "eval",
            &[
                ("model_descriptor", GGUF),
                ("runtime_descriptor", LLAMACPP),
                ("model_flags", stored),
            ],
        )?;
        assert_eq!(
            body.get("model_flags"),
            Some(&serde_json::json!({
                "benchmark_type": "eval",
                "model_type": "gguf_text",
                "enable_thinking": thinking,
            }))
        );
        Ok(())
    }

    /// A `reasoning_effort` follows `enable_thinking` in that spelling, or
    /// stands alone, and reads back beside it.
    #[rstest::rstest]
    #[case::after_thinking(
        "enable_thinking=false,reasoning_effort=low",
        serde_json::json!({
            "benchmark_type": "eval",
            "model_type": "gguf_text",
            "enable_thinking": false,
            "reasoning_effort": "low",
        })
    )]
    #[case::alone(
        "reasoning_effort=high",
        serde_json::json!({
            "benchmark_type": "eval",
            "model_type": "gguf_text",
            "reasoning_effort": "high",
        })
    )]
    fn a_reported_reasoning_effort_reads_as_model_flags(
        #[case] stored: &str,
        #[case] expected: Value,
    ) -> anyhow::Result<()> {
        let body = body_of(
            "eval",
            &[
                ("model_descriptor", GGUF),
                ("runtime_descriptor", LLAMACPP),
                ("model_flags", stored),
            ],
        )?;
        assert_eq!(body.get("model_flags"), Some(&expected));
        Ok(())
    }

    /// Without a model to name the cell, the text stays what it is.
    #[test]
    fn a_canonical_string_without_a_model_reads_as_unknown() -> anyhow::Result<()> {
        let body = body_of("eval", &[("model_flags", "enable_thinking=true")])?;
        assert_eq!(
            body.get("model_flags"),
            Some(&serde_json::json!({"type": "unknown", "value": "enable_thinking=true"}))
        );
        Ok(())
    }

    /// What does not decode is the stored value as it is: the JSON, or the text.
    #[rstest::rstest]
    #[case::cli_string("runtime_flags", "--mmap 0 -fa 1", serde_json::json!("--mmap 0 -fa 1"))]
    #[case::unknown_field(
        "requested_runtime_flags",
        r#"{"benchmark_type":"prefill_throughput","model_type":"gguf_text","runtime_type":"llamacpp_cli_stock_tools","speculative_envs":["A=1"]}"#,
        serde_json::json!({
            "benchmark_type": "prefill_throughput",
            "model_type": "gguf_text",
            "runtime_type": "llamacpp_cli_stock_tools",
            "speculative_envs": ["A=1"],
        })
    )]
    #[case::unknown_variant(
        "runtime_descriptor",
        r#"{"type":"catalog_defined","name":"x"}"#,
        serde_json::json!({"name": "x", "type": "catalog_defined"})
    )]
    #[case::not_an_object("client_extras", "[1]", serde_json::json!([1]))]
    fn a_value_that_does_not_decode_reads_as_unknown(
        #[case] column: &str,
        #[case] stored: &str,
        #[case] value: Value,
    ) -> anyhow::Result<()> {
        let body = body_of(
            "prefill_throughput",
            &[("model_descriptor", GGUF), (column, stored)],
        )?;
        assert_eq!(
            body.get(column),
            Some(&serde_json::json!({"type": "unknown", "value": value}))
        );
        Ok(())
    }

    #[test]
    fn rows_group_by_job_in_order() -> anyhow::Result<()> {
        let result: QueryResult = serde_json::from_str(
            r#"{
                "columns": ["job_id", "benchmark_id", "benchmark_type", "metric", "value",
                            "value_stddev", "unit"],
                "rows": [["job-1", "prefill_throughput_256", "prefill_throughput", "prefill",
                          812.5, null, "tok/s"],
                         ["job-2", "prefill_throughput_256", "prefill_throughput", "prefill",
                          700.0, null, "tok/s"],
                         ["job-1", "prefill_throughput_256", "prefill_throughput", "ttft",
                          0.4, null, "s"]]
            }"#,
        )?;
        let jobs = rows_by_job(parse_rows(result)?);
        let metrics = |job_id: &str| -> anyhow::Result<Vec<String>> {
            Ok(jobs
                .get(&JobId::try_new(job_id)?)
                .map(|rows| rows.iter().map(|row| row.metric.clone()).collect())
                .unwrap_or_default())
        };
        assert_eq!(metrics("job-1")?, ["prefill", "ttft"]);
        assert_eq!(metrics("job-2")?, ["prefill"]);
        assert_eq!(jobs.len(), 2);
        Ok(())
    }

    fn db(url: &str) -> Result<ResultsDb, ResultsError> {
        ResultsDb::new(
            reqwest::Client::new(),
            DuckDbConfig {
                url: url.to_string(),
                user: "mgmt".to_string(),
                password: pipette_core::config::DuckDbPassword::new("secret"),
                database: "internal".to_string(),
                timeout_secs: std::num::NonZeroU64::MIN,
            },
        )
    }

    #[test]
    fn a_bad_url_fails_at_construction_not_per_read() {
        assert!(matches!(db("not a url"), Err(ResultsError::Url(_))));
    }

    /// Refused before any request: the port is closed, so a query would be `Unreachable`.
    #[tokio::test]
    async fn a_window_past_bigint_is_refused_before_querying() -> anyhow::Result<()> {
        let past = usize::try_from(i64::MAX)? + 1;
        let refused = db("http://127.0.0.1:9")?
            .plan_results(&plan()?, past, 10)
            .await;
        assert!(matches!(refused, Err(ResultsError::Window { .. })));
        Ok(())
    }

    /// The port is closed, so a query would be `Unreachable`.
    #[tokio::test]
    async fn no_jobs_read_no_rows_without_querying() -> anyhow::Result<()> {
        assert!(db("http://127.0.0.1:9")?.job_results(&[]).await?.is_empty());
        Ok(())
    }

    #[test]
    fn counts_read_the_one_row() -> anyhow::Result<()> {
        let result: QueryResult =
            serde_json::from_str(r#"{"columns": ["total", "jobs"], "rows": [[7, 3]]}"#)?;
        assert_eq!(parse_counts(result)?, (7, 3));
        Ok(())
    }

    #[rstest::rstest]
    #[case::all_plans(true)]
    #[case::current_plan(false)]
    fn the_device_time_query_counts_each_job_of_each_plan_once(
        #[case] skip_rescored: bool,
    ) -> anyhow::Result<()> {
        let other = PlanId::try_new("plan-0192b3c4-0000-7000-8000-000000000002")?;
        let sql = device_time_sql(&[plan()?, other], skip_rescored);
        assert!(
            sql.contains(
                "FROM (SELECT DISTINCT job_id FROM plan_jobs WHERE plan_id IN \
                 ('plan-0192b3c4-0000-7000-8000-000000000001', \
                 'plan-0192b3c4-0000-7000-8000-000000000002')) pj"
            ),
            "{sql}"
        );
        assert!(sql.contains("any_value(device_time_ms)"), "{sql}");
        assert!(
            sql.contains("(failure_source IS NULL OR failure_source = 'client')"),
            "a failure that the server declared is not unreported: {sql}"
        );
        assert!(
            sql.contains("AND client_id <> 'system') AS unreported_job_count"),
            "a server failure stored before `failure_source` is not unreported: {sql}"
        );
        assert!(sql.contains("GROUP BY job_id"), "{sql}");
        assert!(
            sql.starts_with(
                "SELECT CAST(CASE WHEN sum(device_time_ms) > 9223372036854775807 \
                 THEN 9223372036854775807 ELSE sum(device_time_ms) END AS BIGINT)"
            ),
            "{sql}"
        );
        assert_eq!(
            sql.contains("rescored_from IS NULL"),
            skip_rescored,
            "{sql}"
        );
        assert!(!sql.contains(';'));
        Ok(())
    }

    /// `None` where the row is refused.
    #[rstest::rstest]
    #[case::summed("[[184250, 2]]", Some((Some(184_250), 2)))]
    #[case::none_reported("[[null, 3]]", Some((None, 3)))]
    #[case::no_row("[]", None)]
    #[case::not_a_number(r#"[["184250", 0]]"#, None)]
    #[case::negative("[[-1, 0]]", None)]
    fn a_device_time_total_reads_the_one_row(
        #[case] rows: &str,
        #[case] expected: Option<(Option<i64>, usize)>,
    ) -> anyhow::Result<()> {
        let result: QueryResult = serde_json::from_str(&format!(
            r#"{{"columns": ["device_time_ms", "unreported_job_count"], "rows": {rows}}}"#
        ))?;
        let parsed = parse_device_time(result).ok().map(|total| {
            (
                total.device_time_ms.map(ElapsedMillis::into_inner),
                total.unreported_job_count,
            )
        });
        assert_eq!(parsed, expected);
        Ok(())
    }

    /// The port is closed, so a query would be `Unreachable`.
    #[tokio::test]
    async fn no_plans_total_nothing_without_querying() -> anyhow::Result<()> {
        let total = db("http://127.0.0.1:9")?
            .device_time_total(&[], true)
            .await?;
        assert_eq!(total, DeviceTimeTotal::default());
        Ok(())
    }
}
