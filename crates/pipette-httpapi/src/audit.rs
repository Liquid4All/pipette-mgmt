//! One audit event for each request to a write route, refused ones included
//! (`pipette_core::audit`). The extractors name the caller, the path names the
//! records acted on, and a handler names what it made or moved.

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{FromRequestParts, MatchedPath, RawPathParams, State};
use axum::http::request::Parts;
use axum::http::{HeaderMap, Method, Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;
use futures::stream::{self, StreamExt};

use pipette_core::audit::{new_event, outcome_of_status};
use pipette_pipeline::audit::AuditReach;
use pipette_plan_types::{
    AuditAction, AuditActor, AuditEvent, AuditOutcome, AuditTarget, CampaignId, ClientId, JobId,
    OrgId, PlanId, UserId, VaultPrincipalId, VaultResourceId,
};

use crate::state::AppState;

/// The longest claimed id an unverified event keeps.
const CLAIM_MAX: usize = 128;

/// Status of a streamed answer whose caller left before its last line.
pub(crate) const CALLER_LEFT: u16 = 499;

/// The longest time a plain answer waits for its audit event to be written.
/// After it, the answer goes out and the write continues on its own.
const WRITE_WAIT: Duration = Duration::from_secs(1);

/// The write routes whose requests make no audit event. A device renews its
/// lease at each half of the lease time, and a renewal changes nothing that an
/// operator audits.
pub const UNAUDITED: &[&str] = &["/plans/{job_id}/heartbeat"];

/// What the request has said about itself so far, shared by the middleware,
/// the extractors, the handler and any writes it detaches.
#[derive(Clone, Default)]
struct Slot(Arc<Mutex<Draft>>);

/// What the event will say.
#[derive(Default)]
struct Facts {
    actor: Option<AuditActor>,
    targets: BTreeSet<AuditTarget>,
    orgs: BTreeSet<OrgId>,
    unchanged: bool,
    /// The answer is a `404` that hides a record outside the caller's scope.
    out_of_scope: bool,
}

/// The event as it stands. The middleware writes the event of a plain answer.
/// For any other request, the last holder of the slot writes the event: the
/// end of a stream, a write that continues after its caller left, or a request
/// that stopped before its answer.
#[derive(Default)]
struct Draft {
    facts: Facts,
    /// The opened event and the state to write it with, until it is written.
    pending: Option<Box<Pending>>,
    streaming: bool,
    /// The status of a streamed answer's last line.
    settled: Option<u16>,
    /// What detached writes reached: they stand even when the caller left.
    committed: Option<u16>,
}

struct Pending {
    state: AppState,
    event: AuditEvent,
}

/// The last holder writes the event here. It reads no out-of-scope mark: every
/// `AppError::OutOfScope` is raised before a stream opens or a write detaches,
/// so the middleware's plain path sees it.
impl Drop for Draft {
    fn drop(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        let status = self.committed.or(self.settled).unwrap_or(CALLER_LEFT);
        let facts = std::mem::take(&mut self.facts);
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                let writes = pending.state.audit_writes.clone();
                drop(writes.spawn_on(write(*pending, facts, status), &runtime));
            }
            Err(_) => tracing::error!("no runtime left to write an audit event; it is lost"),
        }
    }
}

impl Slot {
    fn with<T>(&self, f: impl FnOnce(&mut Draft) -> T) -> T {
        f(&mut self.0.lock().unwrap_or_else(PoisonError::into_inner))
    }
}

/// A handler's hold on its request's audit event; a read route's records
/// nothing.
#[derive(Clone, Default)]
pub struct Audit(Option<Slot>);

impl<S: Send + Sync> FromRequestParts<S> for Audit {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<Slot>().cloned()))
    }
}

impl Audit {
    fn with(&self, f: impl FnOnce(&mut Draft)) {
        if let Some(slot) = &self.0 {
            slot.with(f);
        }
    }

    /// Name a record that the request made or acts on. The audit reads the
    /// organization of the record now and again when it writes the event, so
    /// a record that moves is in the logs of both organizations. Call this
    /// before a move and after a create.
    pub async fn involve(&self, state: &AppState, targets: impl IntoIterator<Item = AuditTarget>) {
        if self.0.is_none() {
            return;
        }
        let targets: Vec<AuditTarget> = targets.into_iter().collect();
        let orgs = state.audit_reach().orgs_of(&targets).await;
        self.with(|draft| {
            draft.facts.targets.extend(targets);
            draft.facts.orgs.extend(orgs);
        });
    }

    /// The caller, for a route that authenticates without an extractor: a
    /// registration, once it holds.
    pub fn acted_as(&self, actor: AuditActor) {
        self.with(|draft| draft.facts.actor = Some(actor));
    }

    /// The request succeeded and changed nothing: a dry run, a check, or a claim
    /// that found no job. A refusal or a failure is recorded regardless.
    pub fn unchanged(&self) {
        self.with(|draft| draft.facts.unchanged = true);
    }

    /// [`Self::unchanged`] unless `changed`, for a write that reports it.
    pub fn changed(&self, changed: bool) {
        if !changed {
            self.unchanged();
        }
    }

    /// The answer streams, so the event waits for its last holder.
    pub(crate) fn stream(&self) {
        self.with(|draft| draft.streaming = true);
    }

    /// The streamed answer ended with `status`: the result line's, or
    /// [`CALLER_LEFT`].
    pub(crate) fn settle(&self, status: u16) {
        self.with(|draft| draft.settled = Some(status));
    }

    /// Writes detached from the request ended with `status`, whether or not the
    /// caller is still there to hear it.
    pub(crate) fn committed(&self, status: u16) {
        self.with(|draft| draft.committed = Some(status));
    }

    /// Run `work` on its own task. If the event is not written yet, the task
    /// goes on the tracker that `serve` waits on at shutdown, so work that
    /// still owes the event is not cut off.
    pub(crate) fn spawn<F>(&self, work: F) -> tokio::task::JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let tracker = self.0.as_ref().and_then(|slot| {
            slot.with(|draft| {
                draft
                    .pending
                    .as_ref()
                    .map(|pending| pending.state.audit_writes.clone())
            })
        });
        match tracker {
            Some(tracker) => tracker.spawn(work),
            None => tokio::spawn(work),
        }
    }
}

/// Write an event of its own beside the request's, for what a request found
/// rather than what it asked: in the log of each target's organization.
pub(crate) async fn append(
    state: &AppState,
    actor: AuditActor,
    action: AuditAction,
    targets: BTreeSet<AuditTarget>,
) {
    let Some(store) = state.audit_store.as_deref() else {
        return;
    };
    let mut event = new_event(actor, action, AuditOutcome::Succeeded);
    event.org_ids = state.audit_reach().orgs_of(&targets).await;
    event.targets = targets;
    if let Err(e) = store.append(&event).await {
        tracing::error!(event_id = %event.event_id, error = %format!("{e:#}"),
            "failed to write the audit event");
    }
}

/// The mark that [`crate::error::AppError::OutOfScope`] puts on its answer: the
/// `404` that hides a record outside the caller's scope.
#[derive(Clone, Copy)]
pub(crate) struct OutOfScope;

/// Note the caller an authentication extractor found. When the extractor
/// refused, the claim the request made stands: its `X-Client-Id`, or the key
/// id [`note_claim`] named.
pub(crate) fn note_caller(parts: &Parts, verified: Option<AuditActor>) {
    let Some(slot) = parts.extensions.get::<Slot>() else {
        return;
    };
    slot.with(|draft| match verified {
        Some(actor) => draft.facts.actor = Some(actor),
        None => {
            draft
                .facts
                .actor
                .get_or_insert(AuditActor::Unverified { claimed_id: None });
        }
    });
}

/// Note the principal a request's signature names before it is checked, for a
/// scheme that names it nowhere else (RFC 9421's key id).
pub(crate) fn note_claim(parts: &Parts, claimed: &str) {
    if let Some(slot) = parts.extensions.get::<Slot>() {
        let actor = AuditActor::Unverified {
            claimed_id: claim(Some(claimed)),
        };
        slot.with(|draft| draft.facts.actor = Some(actor));
    }
}

/// A claimed id worth keeping: an id's charset, and short.
fn claim(raw: Option<&str>) -> Option<String> {
    raw.filter(|claimed| claimed.len() <= CLAIM_MAX && ClientId::try_new(*claimed).is_ok())
        .map(str::to_owned)
}

impl AppState {
    pub fn audit_reach(&self) -> AuditReach<'_> {
        AuditReach {
            orgs: self.org_store.as_ref(),
            campaigns: self.campaign_store.as_ref(),
            plans: self.plan_store.as_ref(),
            vault: &self.vault_store,
        }
    }
}

/// The middleware, as a route layer: only a matched route has a pattern.
pub(crate) async fn record(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    if matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) {
        return next.run(request).await;
    }
    // Without a [database] there is no log, so no handler is handed a slot.
    if state.audit_store.is_none() {
        return next.run(request).await;
    }
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|route| route.as_str().to_owned())
        .unwrap_or_default();
    if UNAUDITED.contains(&route.as_str()) {
        return next.run(request).await;
    }
    let (mut parts, body) = request.into_parts();
    let params: Vec<(String, String)> = RawPathParams::from_request_parts(&mut parts, &state)
        .await
        .map(|params| {
            params
                .iter()
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .collect()
        })
        .unwrap_or_default();
    let (method, path) = (parts.method.to_string(), parts.uri.path().to_owned());
    let action = AuditAction::Http {
        method,
        route,
        path,
        status: CALLER_LEFT,
    };
    let event = new_event(
        AuditActor::Unverified { claimed_id: None },
        action,
        AuditOutcome::Failed,
    );
    // A claim stands until an extractor proves the caller, so a refusal on a
    // route that authenticates in its handler keeps it too.
    let claimed = claim(
        parts
            .headers
            .get("X-Client-Id")
            .and_then(|value| value.to_str().ok()),
    );
    let reach = state.audit_reach();
    let targets = path_targets(&reach, &params).await;
    // Read before the handler, so a record that moves is in both logs. A
    // request with no credentials can only be unverified, and an unverified
    // event is in no organization's log, so it skips the reads.
    let orgs = if carries_credentials(&parts.headers) {
        reach.orgs_of(&targets).await
    } else {
        BTreeSet::new()
    };
    let slot = Slot::default();
    slot.with(|draft| {
        draft.facts.actor = claimed.map(|claimed_id| AuditActor::Unverified {
            claimed_id: Some(claimed_id),
        });
        draft.facts.targets.extend(targets);
        draft.facts.orgs.extend(orgs);
        draft.pending = Some(Box::new(Pending {
            state: state.clone(),
            event,
        }));
    });
    parts.extensions.insert(slot.clone());

    let response = next.run(Request::from_parts(parts, body)).await;
    let status = response.status();
    let out_of_scope = response.extensions().get::<OutOfScope>().is_some();
    let plain = slot.with(|draft| {
        // No handler served it: the route answers another method.
        if status == StatusCode::METHOD_NOT_ALLOWED {
            draft.pending = None;
            return None;
        }
        // A refusal before the stream opened answers plainly.
        if draft.streaming && status.is_success() {
            return None;
        }
        let pending = draft.pending.take()?;
        let mut facts = std::mem::take(&mut draft.facts);
        facts.out_of_scope = out_of_scope;
        Some((pending, facts))
    });
    if let Some((pending, facts)) = plain {
        // Its own task, so a caller that leaves now cannot cut the write short.
        let written = state
            .audit_writes
            .spawn(write(*pending, facts, status.as_u16()));
        match tokio::time::timeout(WRITE_WAIT, written).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::error!(error = %e, "the audit write did not finish"),
            Err(_) => tracing::warn!(
                wait_ms = WRITE_WAIT.as_millis(),
                "audit write still running after the wait; the answer goes out first"
            ),
        }
    }
    response
}

/// Fill the event from `facts` and write it, unless it changed nothing or no
/// caller is known. An unverified event is in no organization's log: the id
/// it claims and the path it names are the caller's choice.
async fn write(pending: Pending, facts: Facts, status: u16) {
    let Pending { state, mut event } = pending;
    let outcome = if facts.out_of_scope {
        AuditOutcome::Refused
    } else {
        outcome_of_status(status)
    };
    if facts.unchanged && outcome == AuditOutcome::Succeeded {
        return;
    }
    let actor = facts
        .actor
        .unwrap_or(AuditActor::Unverified { claimed_id: None });
    if actor == (AuditActor::Unverified { claimed_id: None }) && outcome != AuditOutcome::Succeeded
    {
        tracing::debug!(
            status,
            "audit: no event for a failed request that names no caller and claims no id"
        );
        return;
    }
    if let AuditAction::Http { status: field, .. } = &mut event.action {
        *field = status;
    }
    event.outcome = outcome;
    if !matches!(actor, AuditActor::Unverified { .. }) {
        let reach = state.audit_reach();
        event.org_ids = facts.orgs;
        event.org_ids.extend(reach.orgs_of(&facts.targets).await);
        event.org_ids.extend(reach.org_of_actor(&actor).await);
    }
    event.actor = actor;
    event.targets = facts.targets;
    let Some(store) = state.audit_store.as_deref() else {
        return;
    };
    if let Err(e) = store.append(&event).await {
        tracing::error!(event_id = %event.event_id, error = %format!("{e:#}"),
            "failed to write the audit event");
    }
}

/// Whether the request carries a signature or a header that names a caller,
/// which an authentication extractor could verify.
fn carries_credentials(headers: &HeaderMap) -> bool {
    pipette_httpsig::present(headers)
        || [
            "Authorization",
            "X-Client-Id",
            "X-Signature",
            "X-User-Key-Id",
        ]
        .into_iter()
        .any(|name| headers.contains_key(name))
}

/// The records a path's parameters name. A value that does not parse, or a
/// handle that names no organization, names none.
async fn path_targets(reach: &AuditReach<'_>, params: &[(String, String)]) -> Vec<AuditTarget> {
    stream::iter(params)
        .filter_map(|(name, value)| async move {
            let value = value.as_str();
            match name.as_str() {
                "org_id" => reach
                    .org_ref(value)
                    .await
                    .map(|org_id| AuditTarget::Org { org_id }),
                "user_id" => UserId::try_new(value)
                    .ok()
                    .map(|user_id| AuditTarget::User { user_id }),
                "client_id" => ClientId::try_new(value)
                    .ok()
                    .map(|client_id| AuditTarget::Client { client_id }),
                "campaign_id" => CampaignId::try_new(value)
                    .ok()
                    .map(|campaign_id| AuditTarget::Campaign { campaign_id }),
                "plan_id" => PlanId::try_new(value)
                    .ok()
                    .map(|plan_id| AuditTarget::Plan { plan_id }),
                "job_id" => JobId::try_new(value)
                    .ok()
                    .map(|job_id| AuditTarget::Job { job_id }),
                // A model-params key: a vault id names the resource the row describes.
                "resource_id" | "model_key" => VaultResourceId::try_new(value)
                    .ok()
                    .map(|resource_id| AuditTarget::VaultResource { resource_id }),
                "principal_id" => value.parse().ok().map(|principal| match principal {
                    VaultPrincipalId::Org(org_id) => AuditTarget::Org { org_id },
                    VaultPrincipalId::User(user_id) => AuditTarget::User { user_id },
                }),
                _ => None,
            }
        })
        .collect()
        .await
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::CapturingAuditStore;

    use super::*;

    const CLAIMED: &str = "ev1_0123456789abcdef";

    /// A request whose last holder lets go before an answer is still one event:
    /// the caller left, unless writes it detached report what they did. A
    /// request that names no caller and claims no id is no event.
    #[rstest]
    #[case::the_caller_left(Some(CLAIMED), None, Some((AuditOutcome::Failed, CALLER_LEFT)))]
    #[case::a_detached_write_committed(Some(CLAIMED), Some(201), Some((AuditOutcome::Succeeded, 201)))]
    #[case::no_caller_named(None, None, None)]
    #[tokio::test]
    async fn a_request_cut_off_midway(
        #[case] claimed: Option<&str>,
        #[case] committed: Option<u16>,
        #[case] expected: Option<(AuditOutcome, u16)>,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        pipette_web_testkit::setup_benchmarks(dir.path())?;
        let log = Arc::new(CapturingAuditStore::default());
        let config = pipette_web_testkit::test_config(dir.path());
        let stores = Stores {
            audit: Some(log.clone()),
            descriptor_catalog: None,
            ..build_local_fs_stores(&config)?
        };
        let state = AppState::new(config, stores, pipette_web_testkit::test_http_client()?).await?;
        let event = new_event(
            AuditActor::Unverified { claimed_id: None },
            AuditAction::Http {
                method: "POST".into(),
                route: "/api/v1/campaigns".into(),
                path: "/api/v1/campaigns".into(),
                status: 0,
            },
            AuditOutcome::Failed,
        );
        let slot = Slot::default();
        slot.with(|draft| {
            draft.facts.actor = claimed.map(|claimed| AuditActor::Unverified {
                claimed_id: Some(claimed.to_owned()),
            });
            draft.pending = Some(Box::new(Pending {
                state: state.clone(),
                event,
            }));
            draft.committed = committed;
        });
        drop(slot);
        // The write runs on the tracker, so the tracker empties once it ends.
        state.audit_writes.close();
        state.audit_writes.wait().await;
        let written: Vec<(AuditOutcome, u16)> = log
            .events()
            .into_iter()
            .map(|event| match event.action {
                AuditAction::Http { status, .. } => (event.outcome, status),
                AuditAction::Cli { .. } | AuditAction::FirstClerkSignIn { .. } => {
                    (event.outcome, 0)
                }
            })
            .collect();
        assert_eq!(written, Vec::from_iter(expected));
        Ok(())
    }
}
