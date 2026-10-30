//! Which organizations an audit event concerns. Shared by the HTTP layer and
//! the host CLI, so both attribute one target to the same logs.

use std::collections::BTreeSet;

use futures::stream::{self, StreamExt};

use pipette_core::audit::new_event;
use pipette_plan_types::{
    AuditAction, AuditActor, AuditEvent, AuditOutcome, AuditTarget, CampaignId, ClientId, JobId,
    OrgId, PlanId, PrincipalOwner, UserId, VaultResourceId,
};
use pipette_store::{AuditFilter, AuditStores, CampaignStore, OrgStore, PlanStore, VaultRecords};

use crate::org::OrgRef;

/// Why there is no audit log to read or write: no object store keeps one.
pub const NEEDS_DATABASE: &str = "the audit log is kept only on a [database]";

/// Why a listing's filter values name nothing.
#[derive(Debug, thiserror::Error)]
pub enum AuditFilterError {
    #[error("outcome: {0:?} is not one of succeeded, refused, failed")]
    Outcome(String),
    #[error("actor: {0:?} is not one of user, client, unverified, host")]
    Actor(String),
    #[error("target: {0:?} is no record id")]
    Target(String),
}

/// The filter of a listing, from the comma-separated values the HTTP query and
/// the CLI take; an empty list keeps every value.
pub fn audit_filter(
    org_id: Option<OrgId>,
    outcome: Option<&str>,
    actor: Option<&str>,
    target: Option<&str>,
) -> Result<AuditFilter, AuditFilterError> {
    let listed = |raw: Option<&str>| -> Vec<String> {
        raw.unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let outcomes = listed(outcome)
        .into_iter()
        .map(|value| value.parse().map_err(|_| AuditFilterError::Outcome(value)))
        .collect::<Result<_, _>>()?;
    let actor_kinds = listed(actor)
        .into_iter()
        .map(|value| value.parse().map_err(|_| AuditFilterError::Actor(value)))
        .collect::<Result<_, _>>()?;
    // A device registered before the `ev1_` prefix has an id of no shape.
    let record = target
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            target_of_id(value)
                .or_else(|| {
                    ClientId::try_new(value)
                        .ok()
                        .map(|client_id| AuditTarget::Client { client_id })
                })
                .ok_or_else(|| AuditFilterError::Target(value.to_owned()))
        })
        .transpose()?;
    Ok(AuditFilter {
        org_id,
        outcomes,
        actor_kinds,
        record,
    })
}

#[derive(Clone, Copy)]
pub struct AuditReach<'a> {
    pub orgs: &'a dyn OrgStore,
    pub campaigns: &'a dyn CampaignStore,
    pub plans: &'a dyn PlanStore,
    pub vault: &'a dyn VaultRecords,
}

impl<'a> AuditReach<'a> {
    pub fn of(stores: &'a AuditStores) -> Self {
        Self {
            orgs: stores.orgs.as_ref(),
            campaigns: stores.campaigns.as_ref(),
            plans: stores.plans.as_ref(),
            vault: stores.vault.as_ref(),
        }
    }

    /// The organizations `targets` belong to now. A failed read is logged and
    /// skipped: an event with fewer organizations beats no event.
    pub async fn orgs_of<'t>(
        &self,
        targets: impl IntoIterator<Item = &'t AuditTarget>,
    ) -> BTreeSet<OrgId> {
        stream::iter(targets)
            .filter_map(|target| async move {
                self.org_of(target)
                    .await
                    .inspect_err(|e| {
                        tracing::warn!(?target, error = %format!("{e:#}"),
                            "audit: failed to resolve a target's organization");
                    })
                    .ok()
                    .flatten()
            })
            .collect()
            .await
    }

    /// The organization `actor` acts for: an account's, or a device's owner's.
    /// An unverified actor or a host acts for none.
    pub async fn org_of_actor(&self, actor: &AuditActor) -> Option<OrgId> {
        let target = actor.record()?;
        self.orgs_of([&target]).await.pop_first()
    }

    async fn org_of(&self, target: &AuditTarget) -> anyhow::Result<Option<OrgId>> {
        match target {
            AuditTarget::Org { org_id } => Ok(Some(org_id.clone())),
            AuditTarget::User { user_id } => self.orgs.get_membership(user_id).await,
            AuditTarget::Client { client_id } => crate::org::owning_org(self.orgs, client_id).await,
            AuditTarget::Campaign { campaign_id } => self.campaign_org(campaign_id).await,
            AuditTarget::Plan { plan_id } => self.plan_org(plan_id).await,
            AuditTarget::VaultResource { resource_id } => self.vault_org(resource_id).await,
            // A job reaches its plans only through a scan of the queue.
            AuditTarget::Job { .. } => Ok(None),
        }
    }

    async fn owner_org(&self, owner: &PrincipalOwner) -> anyhow::Result<Option<OrgId>> {
        match owner {
            PrincipalOwner::Org { org_id } => Ok(Some(org_id.clone())),
            PrincipalOwner::User { user_id } => self.orgs.get_membership(user_id).await,
        }
    }

    async fn campaign_org(&self, campaign_id: &CampaignId) -> anyhow::Result<Option<OrgId>> {
        match self.campaigns.get_campaign(campaign_id).await? {
            Some(campaign) => self.owner_org(&campaign.owner).await,
            None => Ok(None),
        }
    }

    async fn plan_org(&self, plan_id: &PlanId) -> anyhow::Result<Option<OrgId>> {
        let summaries = self
            .plans
            .get_plan_summaries(std::slice::from_ref(plan_id))
            .await?;
        let Some(plan) = summaries.items.into_iter().next() else {
            return Ok(None);
        };
        match (plan.campaign_id, plan.submitted_by) {
            (Some(campaign_id), _) => self.campaign_org(&campaign_id).await,
            (None, Some(user_id)) => self.orgs.get_membership(&user_id).await,
            (None, None) => Ok(None),
        }
    }

    async fn vault_org(&self, resource_id: &VaultResourceId) -> anyhow::Result<Option<OrgId>> {
        match self.vault.get_resource(resource_id).await? {
            Some(resource) => self.owner_org(&resource.owner).await,
            None => Ok(None),
        }
    }

    /// The organizations that `org_refs` name, ids or handles, as targets. A
    /// handle that names no organization names none.
    async fn named_orgs(&self, org_refs: &[String]) -> Vec<AuditTarget> {
        stream::iter(org_refs)
            .filter_map(|raw| self.org_ref(raw))
            .map(|org_id| AuditTarget::Org { org_id })
            .collect()
            .await
    }

    /// The organization a path's `{org_id}` names, an id or a handle.
    pub async fn org_ref(&self, raw: &str) -> Option<OrgId> {
        match raw.parse::<OrgRef>().ok()? {
            OrgRef::Id(org_id) => Some(org_id),
            OrgRef::Slug(slug) => match self.orgs.lookup_org_by_slug(slug.as_str()).await {
                Ok(org_id) => org_id,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "audit: failed to resolve a handle");
                    None
                }
            },
        }
    }
}

/// A host command that changes records, recorded when it ends. Its targets'
/// organizations are read at the start and again at the end, so a command that
/// moves a record is in both logs.
pub struct CommandAudit {
    event: AuditEvent,
    orgs: BTreeSet<OrgId>,
    /// Read again at the end, so an organization the command created is named.
    org_refs: Vec<String>,
}

impl CommandAudit {
    /// `command` is the subcommand path, such as `users set-org`; `org_refs`
    /// the values of its organization arguments, ids or handles.
    pub async fn start(
        stores: &AuditStores,
        command: String,
        args: Vec<String>,
        org_refs: Vec<String>,
    ) -> Self {
        let reach = AuditReach::of(stores);
        let mut targets: BTreeSet<AuditTarget> = args
            .iter()
            .flat_map(|arg| arg.split(['=', ',']))
            .filter_map(target_of_id)
            .collect();
        targets.extend(reach.named_orgs(&org_refs).await);
        let orgs = reach.orgs_of(&targets).await;
        let mut event = new_event(
            host_actor(),
            AuditAction::Cli { command, args },
            AuditOutcome::Succeeded,
        );
        event.targets = targets;
        Self {
            event,
            orgs,
            org_refs,
        }
    }

    /// Write the event, dated when the command started.
    pub async fn finish(self, stores: &AuditStores, outcome: AuditOutcome) -> anyhow::Result<()> {
        let Self {
            mut event,
            mut orgs,
            org_refs,
        } = self;
        let reach = AuditReach::of(stores);
        event.targets.extend(reach.named_orgs(&org_refs).await);
        orgs.extend(reach.orgs_of(&event.targets).await);
        event.org_ids = orgs;
        event.outcome = outcome;
        stores.log.append(&event).await
    }
}

/// The operator of this process and the host name. The operator is the `sudo`
/// caller, else the account that runs the process. The host name is
/// `$HOSTNAME` if set, else the node name from the kernel.
pub fn host_actor() -> AuditActor {
    let os_user = ["SUDO_USER", "USER", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()));
    let hostname = std::env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            rustix::system::uname()
                .nodename()
                .to_str()
                .ok()
                .map(str::to_owned)
        });
    let known = |value: Option<String>| {
        value
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    };
    AuditActor::Host {
        os_user: known(os_user),
        hostname: known(hostname),
    }
}

/// The target an id names, read by its shape: what a CLI argument or a path
/// segment can carry. A handle or a key id names none.
pub fn target_of_id(raw: &str) -> Option<AuditTarget> {
    let target = if raw.starts_with(OrgId::PREFIX) {
        AuditTarget::Org {
            org_id: OrgId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(UserId::PREFIX) {
        AuditTarget::User {
            user_id: UserId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(CampaignId::PREFIX) {
        AuditTarget::Campaign {
            campaign_id: CampaignId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(VaultResourceId::PREFIX) {
        AuditTarget::VaultResource {
            resource_id: VaultResourceId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(PlanId::MINT_PREFIX) {
        AuditTarget::Plan {
            plan_id: PlanId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(JobId::MINT_PREFIX) {
        AuditTarget::Job {
            job_id: JobId::try_new(raw).ok()?,
        }
    } else if raw.starts_with(ClientId::PREFIX) {
        AuditTarget::Client {
            client_id: ClientId::try_new(raw).ok()?,
        }
    } else {
        return None;
    };
    Some(target)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use anyhow::Context;
    use rstest::{fixture, rstest};

    use pipette_core::organization::Organization;
    use pipette_plan_types::{NonEmptyTrimmedString, OrgSlug};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;
    use pipette_store_testkit::CapturingAuditStore;

    use super::*;

    /// Local stores in a scratch directory, with the audit log captured.
    struct Local {
        _dir: tempfile::TempDir,
        stores: Stores,
        log: Arc<CapturingAuditStore>,
    }

    impl Local {
        fn audit(&self) -> anyhow::Result<AuditStores> {
            Ok(AuditStores {
                orgs: Arc::clone(&self.stores.orgs),
                campaigns: Arc::clone(&self.stores.campaigns),
                plans: Arc::clone(&self.stores.plans),
                vault: Arc::new(Arc::clone(&self.stores.vault)),
                log: Arc::clone(self.stores.audit.as_ref().context("an audit log")?),
            })
        }
    }

    #[fixture]
    fn local() -> anyhow::Result<Local> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let log = Arc::new(CapturingAuditStore::default());
        let stores = Stores {
            audit: Some(log.clone()),
            descriptor_catalog: None,
            ..build_local_fs_stores(&config)?
        };
        Ok(Local {
            _dir: dir,
            stores,
            log,
        })
    }

    async fn org(stores: &Stores, slug: &str) -> anyhow::Result<OrgId> {
        let org = Organization::new(
            OrgSlug::try_new(slug)?,
            NonEmptyTrimmedString::try_new(slug)?,
            chrono::Utc::now(),
        );
        stores
            .orgs
            .claim_org_slug(org.slug.as_str(), &org.org_id)
            .await?;
        stores.orgs.put_org(&org).await?;
        Ok(org.org_id)
    }

    /// A command that moves an account is in the log of the organization it
    /// left and the one it joined, under the host's operator.
    #[rstest]
    #[tokio::test]
    async fn a_command_is_recorded_at_both_ends_of_a_move(
        local: anyhow::Result<Local>,
    ) -> anyhow::Result<()> {
        let local = local?;
        let stores = &local.stores;
        let (alpha, beta) = (org(stores, "alpha").await?, org(stores, "beta").await?);
        let user_id = UserId::try_new("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e")?;
        stores
            .orgs
            .set_membership(&user_id, Some(alpha.clone()))
            .await?;

        let args = ["users", "set-org", user_id.as_str(), "beta"].map(str::to_owned);
        let audit_stores = local.audit()?;
        let audit = CommandAudit::start(
            &audit_stores,
            "users set-org".into(),
            args.to_vec(),
            vec!["beta".to_owned()],
        )
        .await;
        stores
            .orgs
            .set_membership(&user_id, Some(beta.clone()))
            .await?;
        audit.finish(&audit_stores, AuditOutcome::Succeeded).await?;

        let events = local.log.events();
        let [event] = events.as_slice() else {
            anyhow::bail!("{} events", events.len());
        };
        assert!(matches!(event.actor, AuditActor::Host { .. }));
        assert_eq!(
            event.targets,
            BTreeSet::from([
                AuditTarget::User { user_id },
                AuditTarget::Org {
                    org_id: beta.clone()
                }
            ])
        );
        assert_eq!(event.org_ids, BTreeSet::from([alpha, beta]));
        Ok(())
    }

    /// An argument listing ids, `--resource=a,b`, names each.
    #[rstest]
    #[tokio::test]
    async fn a_comma_list_argument_names_each_id(
        local: anyhow::Result<Local>,
    ) -> anyhow::Result<()> {
        let local = local?;
        let args = ["fix-vault-labels", "--resource=vault_a,vault_b"].map(str::to_owned);
        let audit = CommandAudit::start(
            &local.audit()?,
            "fix-vault-labels".into(),
            args.to_vec(),
            vec![],
        )
        .await;
        let named: BTreeSet<AuditTarget> = ["vault_a", "vault_b"]
            .into_iter()
            .map(|id| {
                VaultResourceId::try_new(id)
                    .map(|resource_id| AuditTarget::VaultResource { resource_id })
            })
            .collect::<Result<_, _>>()?;
        assert_eq!(audit.event.targets, named);
        Ok(())
    }

    /// The kernel names the host where no `$HOSTNAME` is exported, as on macOS
    /// and under cron.
    #[test]
    fn the_host_actor_names_its_host() {
        let actor = host_actor();
        assert!(
            matches!(&actor, AuditActor::Host { hostname, .. } if hostname != "unknown"),
            "{actor:?}"
        );
    }

    /// Comma lists, blanks skipped.
    #[test]
    fn a_filter_reads_comma_lists() -> anyhow::Result<()> {
        let filter = audit_filter(
            None,
            Some("refused, failed,"),
            Some("user"),
            Some(" ev1_ab "),
        )?;
        assert_eq!(
            filter.outcomes,
            BTreeSet::from([AuditOutcome::Refused, AuditOutcome::Failed])
        );
        assert_eq!(filter.actor_kinds.len(), 1);
        assert_eq!(
            filter.record,
            Some(AuditTarget::Client {
                client_id: ClientId::try_new("ev1_ab")?
            })
        );
        assert_eq!(
            audit_filter(None, Some(""), None, Some(""))?,
            AuditFilter::default()
        );
        let legacy = audit_filter(None, None, None, Some("macbook-pro-2"))?;
        assert_eq!(
            legacy.record,
            Some(AuditTarget::Client {
                client_id: ClientId::try_new("macbook-pro-2")?
            }),
            "a device registered before the prefix"
        );
        Ok(())
    }

    /// A value naming nothing is refused, not ignored.
    #[rstest]
    #[case::an_unknown_outcome(Some("ok"), None, None)]
    #[case::an_unknown_actor(None, Some("bot"), None)]
    #[case::no_record_id(None, None, Some("a b"))]
    fn a_filter_refuses_a_value_naming_nothing(
        #[case] outcome: Option<&str>,
        #[case] actor: Option<&str>,
        #[case] target: Option<&str>,
    ) {
        assert!(audit_filter(None, outcome, actor, target).is_err());
    }

    #[rstest]
    #[case("org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", Some("org"))]
    #[case("user_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", Some("user"))]
    #[case("campaign_0195c3e2", Some("campaign"))]
    #[case("vault_0195c3e2", Some("vault_resource"))]
    #[case("plan-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", Some("plan"))]
    #[case("job-0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e", Some("job"))]
    #[case("ev1_0123456789abcdef", Some("client"))]
    #[case("userkey_0195c3e2a4f37b8c", None)]
    #[case("acme", None)]
    #[case("--dry-run", None)]
    fn an_id_names_a_target_by_its_shape(#[case] raw: &str, #[case] kind: Option<&str>) {
        assert_eq!(target_of_id(raw).map(|target| target.kind()), kind);
    }
}
