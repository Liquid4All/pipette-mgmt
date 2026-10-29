//! The organization joins: who belongs to one, what it owns, and what has to be
//! true before it or an owning account can be deleted. The reverse lookups scan
//! the two relation keyspaces rather than keep an index.

use std::collections::{HashMap, HashSet};

use futures::TryStreamExt;
use futures::stream;
use futures::stream::StreamExt;

use pipette_core::client::Client;
use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_core::user::User;
use pipette_plan_types::{
    BenchmarkId, CampaignId, CatalogName, ClientId, ClientOwner, ClientPreauthKeyId, OrgId,
    OrgSlug, PrincipalOwner, UserId, VaultResourceId,
};
use pipette_store::{
    CampaignStore, CatalogStore, ClientStore, OrgStore, STORAGE_CONCURRENCY, UserStore, VaultStore,
    drain_campaigns, drain_clients, warn_unreadable_clients,
};

/// An organization by minted id or by handle. The `org_` prefix decides, and a
/// slug carrying it is refused at creation, so no value is both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OrgRef {
    Id(OrgId),
    Slug(OrgSlug),
}

/// Why a value names no organization: a malformed id or a malformed handle.
#[derive(Debug, thiserror::Error)]
pub enum OrgRefError {
    #[error("invalid organization id: {0}")]
    Id(#[from] pipette_plan_types::PrincipalIdError),
    #[error("invalid organization handle: {0}")]
    Slug(#[from] pipette_plan_types::OrgSlugError),
}

impl std::str::FromStr for OrgRef {
    type Err = OrgRefError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.trim().starts_with(OrgId::PREFIX) {
            return Ok(OrgRef::Id(OrgId::try_new(s.trim())?));
        }
        Ok(OrgRef::Slug(OrgSlug::try_new(s)?))
    }
}

impl std::fmt::Display for OrgRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Id(id) => write!(f, "{id}"),
            Self::Slug(slug) => write!(f, "{slug}"),
        }
    }
}

/// The organization `reference` names, or `None`; a handle claim whose record
/// is missing reads as absent.
pub async fn resolve(
    orgs: &dyn OrgStore,
    reference: &OrgRef,
) -> anyhow::Result<Option<Organization>> {
    let org_id = match reference {
        OrgRef::Id(id) => id.clone(),
        OrgRef::Slug(slug) => match orgs.lookup_org_by_slug(slug.as_str()).await? {
            Some(id) => id,
            None => return Ok(None),
        },
    };
    orgs.get_org(&org_id).await
}

/// Why [`require`] produced no organization. Typed, so this crate names no web
/// tier: `pipette_httpapi::error::AppError` maps it to a status.
#[derive(Debug, thiserror::Error)]
pub enum OrgLookupError {
    #[error("no organization {0}")]
    NotFound(OrgRef),
    #[error(transparent)]
    Store(#[from] anyhow::Error),
}

/// [`resolve`], as an error when nothing matches: one message for every surface.
pub async fn require(
    orgs: &dyn OrgStore,
    reference: &OrgRef,
) -> Result<Organization, OrgLookupError> {
    resolve(orgs, reference)
        .await?
        .ok_or_else(|| OrgLookupError::NotFound(reference.clone()))
}

/// Every account belonging to `org_id`, in the roster's creation order.
pub async fn members(
    orgs: &dyn OrgStore,
    users: &dyn UserStore,
    org_id: &OrgId,
) -> anyhow::Result<Vec<User>> {
    let ids = member_ids(orgs, org_id).await?;
    Ok(users
        .list_users()
        .await?
        .into_iter()
        .filter(|user| ids.contains(&user.user_id))
        .collect())
}

async fn member_ids(orgs: &dyn OrgStore, org_id: &OrgId) -> anyhow::Result<HashSet<UserId>> {
    Ok(orgs
        .list_memberships()
        .await?
        .into_iter()
        .filter(|membership| &membership.org_id == org_id)
        .map(|membership| membership.user_id)
        .collect())
}

/// Which organization owns `client_id`, following a user-owned device through
/// its owner's membership: derived, so moving an account moves its devices.
pub async fn owning_org(
    orgs: &dyn OrgStore,
    client_id: &ClientId,
) -> anyhow::Result<Option<OrgId>> {
    match orgs.get_ownership(client_id).await? {
        None => Ok(None),
        Some(owner) => org_of(orgs, &owner).await,
    }
}

/// [`owning_org`] for an ownership already read.
pub async fn org_of(orgs: &dyn OrgStore, owner: &ClientOwner) -> anyhow::Result<Option<OrgId>> {
    match owner {
        ClientOwner::Org { org_id } => Ok(Some(org_id.clone())),
        ClientOwner::User { user_id } => orgs.get_membership(user_id).await,
    }
}

/// Whether `owner` still names a record. A pre-auth key's owner was resolved
/// when the key was minted, and the key can outlive it.
pub async fn owner_exists(
    orgs: &dyn OrgStore,
    users: &dyn UserStore,
    owner: &ClientOwner,
) -> anyhow::Result<bool> {
    Ok(match owner {
        ClientOwner::Org { org_id } => orgs.get_org(org_id).await?.is_some(),
        ClientOwner::User { user_id } => users.get_user(user_id).await?.is_some(),
    })
}

/// How many members and devices each organization holds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrgCounts {
    pub members: usize,
    pub clients: usize,
}

/// Counts for every organization from one pass over each relation keyspace.
/// A reference to an organization not in `orgs` is counted nowhere.
pub fn tally(
    orgs: &[Organization],
    memberships: &[Membership],
    ownerships: &[Ownership],
) -> HashMap<OrgId, OrgCounts> {
    let mut counts: HashMap<OrgId, OrgCounts> = orgs
        .iter()
        .map(|org| (org.org_id.clone(), OrgCounts::default()))
        .collect();

    // Built once, so each user-owned device is a lookup, not a membership scan.
    let org_of_user: HashMap<&UserId, &OrgId> = memberships
        .iter()
        .map(|membership| (&membership.user_id, &membership.org_id))
        .collect();

    // The `get_mut` stays inside each closure: a `&mut` yielded out of one
    // would outlive the `FnMut` borrow.
    memberships.iter().for_each(|membership| {
        if let Some(entry) = counts.get_mut(&membership.org_id) {
            entry.members += 1;
        }
    });

    ownerships
        .iter()
        .filter_map(|ownership| match &ownership.owner {
            ClientOwner::Org { org_id } => Some(org_id),
            ClientOwner::User { user_id } => org_of_user.get(user_id).copied(),
        })
        .for_each(|org_id| {
            if let Some(entry) = counts.get_mut(org_id) {
                entry.clients += 1;
            }
        });

    counts
}

/// The ownership behind every device `org_id` owns, directly or through a
/// member, by device: [`owned_clients`] without the fleet read.
pub async fn ownerships_of(
    orgs: &dyn OrgStore,
    org_id: &OrgId,
) -> anyhow::Result<HashMap<ClientId, ClientOwner>> {
    let members = member_ids(orgs, org_id).await?;
    Ok(orgs
        .list_ownerships()
        .await?
        .into_iter()
        .filter(|ownership| match &ownership.owner {
            ClientOwner::Org { org_id: owner } => owner == org_id,
            ClientOwner::User { user_id } => members.contains(user_id),
        })
        .map(|ownership| (ownership.client_id, ownership.owner))
        .collect())
}

/// Every device `org_id` owns, directly or through its members, with the
/// ownership that puts it there. An unreadable client is warned about and
/// skipped, so one corrupt record cannot hide the rest.
pub async fn owned_clients(
    clients: &dyn ClientStore,
    orgs: &dyn OrgStore,
    org_id: &OrgId,
) -> anyhow::Result<Vec<(Client, ClientOwner)>> {
    let mut owned = ownerships_of(orgs, org_id).await?;

    let listing = drain_clients(clients).await?;
    warn_unreadable_clients(&listing.unreadable);
    Ok(listing
        .items
        .into_iter()
        .filter_map(|client| {
            let owner = owned.remove(&client.client_id)?;
            Some((client, owner))
        })
        .collect())
}

/// Every device owned by `user_id`: with [`holdings_of`], what an account
/// deletion has to answer for.
pub async fn devices_owned_by(
    orgs: &dyn OrgStore,
    user_id: &UserId,
) -> anyhow::Result<Vec<ClientId>> {
    Ok(orgs
        .list_ownerships()
        .await?
        .into_iter()
        .filter(|ownership| ownership.owner.user_id() == Some(user_id))
        .map(|ownership| ownership.client_id)
        .collect())
}

/// Clear the ownership of every device `user_id` owns, so the account can be
/// deleted without stranding one. Re-running finishes a partial pass.
pub async fn detach_devices_of(
    orgs: &dyn OrgStore,
    user_id: &UserId,
) -> anyhow::Result<Vec<ClientId>> {
    let owned = devices_owned_by(orgs, user_id).await?;

    stream::iter(owned.iter().map(|client_id| async move {
        // Handed to someone else since the scan: not ours to clear.
        let owner = orgs.get_ownership(client_id).await?;
        if owner.as_ref().and_then(ClientOwner::user_id) != Some(user_id) {
            return Ok(());
        }
        orgs.delete_ownership(client_id).await?;
        tracing::debug!(%user_id, %client_id, "user device detached");
        anyhow::Ok(())
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await?;

    if !owned.is_empty() {
        tracing::info!(%user_id, count = owned.len(), "user devices detached");
    }
    Ok(owned)
}

/// How far a caller's device authority reaches. A type, not a bool: an org
/// admin's answer depends on the device and on where it is being moved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceScope {
    /// Every device, whoever holds it: what
    /// [`ManageAnyClients`](pipette_plan_types::UserPermission::ManageAnyClients)
    /// grants.
    Fleet,
    /// What [`ManageClients`](pipette_plan_types::UserPermission::ManageClients)
    /// grants, bounded by the caller's own membership.
    Org(OrgId),
    /// Holds the verb but belongs to no organization, so administers nothing: a
    /// state, not an error, since a grant can precede any membership.
    Unaffiliated,
}

impl DeviceScope {
    /// Whether this scope may administer a device that rolls up to `owning_org`.
    /// `None` (unowned, or owned outside every organization) is no org's to take.
    pub fn may_administer(&self, owning_org: Option<&OrgId>) -> bool {
        match self {
            Self::Fleet => true,
            Self::Unaffiliated => false,
            Self::Org(scope) => owning_org == Some(scope),
        }
    }

    /// Whether this scope may hand a device **to** `target`: the other end of a
    /// move from [`Self::may_administer`], so a move never widens another roster.
    pub fn may_assign_to(&self, target: Option<&OrgId>) -> bool {
        match self {
            Self::Fleet => true,
            Self::Unaffiliated => false,
            // Detaching is inside the scope: it removes a device from this
            // organization, which is never a grab.
            Self::Org(scope) => target.is_none_or(|org_id| org_id == scope),
        }
    }

    /// [`Self::may_assign_to`] for a **person** in `owner_org`, with `None` closed:
    /// a user in no organization is an outsider, not a detach target.
    pub fn may_hand_to(&self, owner_org: Option<&OrgId>) -> bool {
        match self {
            Self::Fleet => true,
            Self::Unaffiliated => false,
            Self::Org(scope) => owner_org.is_some_and(|org_id| org_id == scope),
        }
    }
}

/// How far a caller's authority over **accounts and organizations** reaches;
/// apart from [`DeviceScope`] since `manage_orgs` is a separate grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RosterScope {
    Fleet,
    Org(OrgId),
    Unaffiliated,
}

impl RosterScope {
    /// Whether this scope covers an account (or organization) whose membership is
    /// `org_id`. Not an unaffiliated one: an org admin could pull in a server admin.
    pub fn covers(&self, org_id: Option<&OrgId>) -> bool {
        match self {
            Self::Fleet => true,
            Self::Unaffiliated => false,
            Self::Org(scope) => org_id.is_some_and(|id| id == scope),
        }
    }

    /// The organization a scoped caller acts inside, or `None` for the fleet.
    /// What a create writes as the new record's membership.
    pub fn own_org(&self) -> Option<&OrgId> {
        match self {
            Self::Org(org_id) => Some(org_id),
            Self::Fleet | Self::Unaffiliated => None,
        }
    }
}

/// The campaigns and vault records an owner holds. Neither moves with a
/// delete, and a minted owner id never resolves again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Holdings {
    pub campaigns: Vec<CampaignId>,
    pub vault_resources: Vec<VaultResourceId>,
}

impl Holdings {
    pub fn is_empty(&self) -> bool {
        self.campaigns.is_empty() && self.vault_resources.is_empty()
    }
}

/// What `owner` holds. An unreadable record is warned about and skipped, as
/// the listings skip it.
pub async fn holdings_of(
    campaigns: &dyn CampaignStore,
    vault: &dyn VaultStore,
    owner: &PrincipalOwner,
) -> anyhow::Result<Holdings> {
    let (campaigns, vault) =
        futures::try_join!(drain_campaigns(campaigns), vault.list_resources_of(owner))?;
    campaigns.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "skipping malformed campaign record");
    });
    vault.unreadable.iter().for_each(|entry| {
        tracing::warn!(%entry, "skipping malformed vault record");
    });
    Ok(Holdings {
        campaigns: campaigns
            .items
            .into_iter()
            .filter(|campaign| &campaign.owner == owner)
            .map(|campaign| campaign.campaign_id)
            .collect(),
        vault_resources: vault
            .items
            .into_iter()
            .map(|resource| resource.resource_id)
            .collect(),
    })
}

/// What still points at an organization — the answer a delete has to be
/// refused on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Dependents {
    /// Accounts whose membership names it.
    pub members: Vec<UserId>,
    /// Devices whose ownership names it **directly**; a member's device is
    /// released by detaching the member.
    pub clients: Vec<ClientId>,
    /// Private benchmarks it keeps, whose ids and pins would outlive a delete
    /// under an id no org-scoped route resolves.
    pub benchmarks: Vec<BenchmarkId>,
    /// Live descriptor catalog names that the caller must tombstone first.
    pub catalog_entries: Vec<CatalogName>,
    /// Campaigns and vault records it owns; `--detach` clears neither.
    pub held: Holdings,
    /// Unexpired client pre-auth keys that would register a device to it.
    pub preauth_keys: Vec<ClientPreauthKeyId>,
}

impl Dependents {
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.clients.is_empty() && self.only_detachable()
    }

    /// Whether everything here is something `--detach` clears: memberships and
    /// direct ownerships.
    pub fn only_detachable(&self) -> bool {
        self.benchmarks.is_empty()
            && self.catalog_entries.is_empty()
            && self.held.is_empty()
            && self.preauth_keys.is_empty()
    }

    /// One sentence naming the non-zero counts, for a `409` body or a CLI
    /// refusal.
    pub fn describe(&self) -> String {
        let parts: Vec<String> = [
            (self.members.len(), "member(s)"),
            (self.clients.len(), "directly-owned client(s)"),
            (self.benchmarks.len(), "private benchmark(s)"),
            (self.catalog_entries.len(), "live catalog entry(s)"),
            (self.held.campaigns.len(), "campaign(s)"),
            (self.held.vault_resources.len(), "vault resource(s)"),
            (self.preauth_keys.len(), "client pre-auth key(s)"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, what)| format!("{count} {what}"))
        .collect();
        let listed = match parts.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} and {last}", rest.join(", ")),
            _ => parts.concat(),
        };
        format!("{listed} still reference it")
    }
}

/// Everything that would be left pointing at a deleted `org_id`. Read before
/// the delete: an org id is minted, so a stale reference is never reported.
pub async fn dependents(
    orgs: &dyn OrgStore,
    catalog: &dyn CatalogStore,
    clients: &dyn ClientStore,
    campaigns: &dyn CampaignStore,
    vault: &dyn VaultStore,
    org_id: &OrgId,
) -> anyhow::Result<Dependents> {
    let owner = PrincipalOwner::Org {
        org_id: org_id.clone(),
    };
    // An expired key registers nothing, so it strands nothing either.
    let now = chrono::Utc::now();
    Ok(Dependents {
        benchmarks: catalog.list_benchmark_ids_of(org_id).await?,
        held: holdings_of(campaigns, vault, &owner).await?,
        preauth_keys: clients
            .list_client_preauth_keys()
            .await?
            .into_iter()
            .filter(|key| key.default_owner.as_ref().and_then(ClientOwner::org_id) == Some(org_id))
            .filter(|key| !key.is_expired(now))
            .map(|key| key.key_id)
            .collect(),
        ..references(orgs, org_id).await?
    })
}

/// The memberships and ownerships naming `org_id`: what [`detach_all`] clears.
async fn references(orgs: &dyn OrgStore, org_id: &OrgId) -> anyhow::Result<Dependents> {
    let members = member_ids(orgs, org_id).await?.into_iter().collect();
    let clients = orgs
        .list_ownerships()
        .await?
        .into_iter()
        .filter(|ownership| ownership.owner.org_id() == Some(org_id))
        .map(|ownership| ownership.client_id)
        .collect();

    Ok(Dependents {
        members,
        clients,
        ..Dependents::default()
    })
}

/// Clear every direct reference to `org_id` so the record can be deleted; its
/// private benchmarks stay. Not atomic, but a re-run converges.
pub async fn detach_all(orgs: &dyn OrgStore, org_id: &OrgId) -> anyhow::Result<Dependents> {
    let found = references(orgs, org_id).await?;

    // Concurrent: each relation costs a read and a delete.
    stream::iter(found.members.iter().map(|user_id| async move {
        // Reassigned since the scan: no longer this organization's reference.
        if orgs.get_membership(user_id).await?.as_ref() != Some(org_id) {
            return Ok(());
        }
        orgs.delete_membership(user_id).await?;
        tracing::debug!(%org_id, %user_id, "organization member detached");
        anyhow::Ok(())
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await?;

    stream::iter(found.clients.iter().map(|client_id| async move {
        let owner = orgs.get_ownership(client_id).await?;
        if owner.as_ref().and_then(ClientOwner::org_id) != Some(org_id) {
            return Ok(());
        }
        orgs.delete_ownership(client_id).await?;
        tracing::debug!(%org_id, %client_id, "organization device detached");
        anyhow::Ok(())
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect::<Vec<()>>()
    .await?;

    if !found.is_empty() {
        tracing::info!(
            %org_id,
            members = found.members.len(),
            clients = found.clients.len(),
            "organization members and devices detached"
        );
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;
    use std::sync::Arc;

    use chrono::Utc;
    use rstest::rstest;

    use pipette_core::benchmark::Benchmark;
    use pipette_core::config::{Config, StorageConfig};
    use pipette_core::organization::Organization;
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        BenchmarkDefinition, ContactEmail, NonEmptyTrimmedString, PrefillThroughputBenchmark,
        PublicKeyHex,
    };
    use pipette_store_fs::{
        LocalFsClientStore, LocalFsOrgStore, LocalFsUserStore, build_local_fs_stores,
    };

    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        clients: Arc<dyn ClientStore>,
        users: Arc<dyn UserStore>,
        orgs: Arc<dyn OrgStore>,
        catalog: Arc<dyn CatalogStore>,
        campaigns: Arc<dyn CampaignStore>,
        vault: Arc<dyn VaultStore>,
    }

    impl Fixture {
        async fn dependents(&self, org_id: &OrgId) -> anyhow::Result<Dependents> {
            dependents(
                &*self.orgs,
                &*self.catalog,
                &*self.clients,
                &*self.campaigns,
                &*self.vault,
                org_id,
            )
            .await
        }
    }

    fn fixture() -> anyhow::Result<Fixture> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().to_path_buf();
        let stores = build_local_fs_stores(&Config {
            storage: StorageConfig::local_fs(root.clone()),
            auth_storage: StorageConfig::local_fs(root.clone()),
            ..Config::default()
        })?;
        Ok(Fixture {
            clients: Arc::new(LocalFsClientStore::new(root.clone())),
            users: Arc::new(LocalFsUserStore::new(root.clone())),
            orgs: Arc::new(LocalFsOrgStore::new(root)),
            catalog: stores.catalog,
            campaigns: stores.campaigns,
            vault: stores.vault,
            _dir: dir,
        })
    }

    async fn make_org(fixture: &Fixture, slug: &str) -> anyhow::Result<Organization> {
        let org = Organization::new(
            OrgSlug::try_new(slug)?,
            NonEmptyTrimmedString::try_new(slug)?,
            Utc::now(),
        );
        fixture
            .orgs
            .claim_org_slug(org.slug.as_str(), &org.org_id)
            .await?;
        fixture.orgs.put_org(&org).await?;
        Ok(org)
    }

    async fn make_user(
        fixture: &Fixture,
        email: &str,
        org_id: Option<&OrgId>,
    ) -> anyhow::Result<User> {
        let user = User::try_new(ContactEmail::try_new(email)?, Utc::now())?;
        fixture.users.put_user(&user).await?;
        if let Some(org_id) = org_id {
            fixture
                .orgs
                .put_membership(&Membership {
                    user_id: user.user_id.clone(),
                    org_id: org_id.clone(),
                })
                .await?;
        }
        Ok(user)
    }

    async fn make_client(
        fixture: &Fixture,
        id: &str,
        owner: Option<ClientOwner>,
    ) -> anyhow::Result<Client> {
        let client = Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new("ab".repeat(32))?,
            organization: NonEmptyTrimmedString::try_new("free text")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("device@example.com")?,
            status: pipette_core::client::ClientStatus::Approved,
            registered_at: Utc::now(),
            device_profile: Default::default(),
            capabilities: Default::default(),
        };
        fixture.clients.put_client(&client).await?;
        if let Some(owner) = owner {
            fixture
                .orgs
                .put_ownership(&Ownership {
                    client_id: client.client_id.clone(),
                    owner,
                })
                .await?;
        }
        Ok(client)
    }

    #[test]
    fn an_org_reference_parses_as_an_id_or_a_handle() -> anyhow::Result<()> {
        let minted = OrgId::mint();
        assert_eq!(
            OrgRef::from_str(minted.as_str()).map_err(anyhow::Error::msg)?,
            OrgRef::Id(minted)
        );
        assert_eq!(
            OrgRef::from_str("Acme").map_err(anyhow::Error::msg)?,
            OrgRef::Slug(OrgSlug::try_new("acme")?),
            "a handle folds, so an operator's capitalization reaches one key"
        );
        // Prefixed but not a valid id: refused rather than silently read as a
        // handle, which would look up something that cannot exist.
        assert!(OrgRef::from_str("org_bad/slash").is_err());
        assert!(OrgRef::from_str("has space").is_err());

        // A handle carrying the prefix is refused at creation; one that merely
        // starts with the letters is unaffected.
        assert!(OrgSlug::try_new("org_acme").is_err());
        assert!(OrgRef::from_str("organisation").is_ok());
        Ok(())
    }

    /// A handle whose record is missing reads as absent.
    #[tokio::test]
    async fn an_organization_resolves_by_id_or_handle() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let org = make_org(&fixture, "acme").await?;

        assert_eq!(
            resolve(&*fixture.orgs, &OrgRef::Id(org.org_id.clone())).await?,
            Some(org.clone())
        );
        assert_eq!(
            resolve(&*fixture.orgs, &OrgRef::Slug(org.slug.clone())).await?,
            Some(org.clone())
        );
        assert!(
            resolve(&*fixture.orgs, &OrgRef::Slug(OrgSlug::try_new("nobody")?))
                .await?
                .is_none()
        );

        // A stranded claim: the handle is taken, the record is not there.
        fixture.orgs.delete_org(&org.org_id).await?;
        assert!(
            resolve(&*fixture.orgs, &OrgRef::Slug(org.slug))
                .await?
                .is_none(),
            "a claim without a record resolves to nothing"
        );
        Ok(())
    }

    #[tokio::test]
    async fn members_are_the_accounts_naming_the_organization() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let other = make_org(&fixture, "other").await?;

        let member = make_user(&fixture, "in@example.com", Some(&acme.org_id)).await?;
        make_user(&fixture, "elsewhere@example.com", Some(&other.org_id)).await?;
        make_user(&fixture, "unaffiliated@example.com", None).await?;

        let found = members(&*fixture.orgs, &*fixture.users, &acme.org_id).await?;
        assert_eq!(
            found.iter().map(|u| &u.user_id).collect::<Vec<_>>(),
            vec![&member.user_id]
        );
        Ok(())
    }

    /// A member's device follows the member's move rather than being pinned at
    /// assignment time.
    #[tokio::test]
    async fn owned_clients_include_members_devices_and_follow_a_move() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let other = make_org(&fixture, "other").await?;
        let member = make_user(&fixture, "member@example.com", Some(&acme.org_id)).await?;

        let rack = make_client(
            &fixture,
            "ev1_rack",
            Some(ClientOwner::Org {
                org_id: acme.org_id.clone(),
            }),
        )
        .await?;
        let laptop = make_client(
            &fixture,
            "ev1_laptop",
            Some(ClientOwner::User {
                user_id: member.user_id.clone(),
            }),
        )
        .await?;
        make_client(&fixture, "ev1_unowned", None).await?;

        let mut owned: Vec<ClientId> =
            owned_clients(&*fixture.clients, &*fixture.orgs, &acme.org_id)
                .await?
                .into_iter()
                .map(|(c, _)| c.client_id)
                .collect();
        owned.sort();
        assert_eq!(
            owned,
            vec![laptop.client_id.clone(), rack.client_id.clone()]
        );

        // The member moves; their device moves with them and the rack does not.
        fixture
            .orgs
            .put_membership(&Membership {
                user_id: member.user_id.clone(),
                org_id: other.org_id.clone(),
            })
            .await?;

        assert_eq!(
            owned_clients(&*fixture.clients, &*fixture.orgs, &acme.org_id)
                .await?
                .into_iter()
                .map(|(c, _)| c.client_id)
                .collect::<Vec<_>>(),
            vec![rack.client_id]
        );
        assert_eq!(
            owned_clients(&*fixture.clients, &*fixture.orgs, &other.org_id)
                .await?
                .into_iter()
                .map(|(c, _)| c.client_id)
                .collect::<Vec<_>>(),
            vec![laptop.client_id]
        );
        Ok(())
    }

    /// The `None` for a member-less owner is not an error.
    #[tokio::test]
    async fn owning_org_follows_the_owner() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let member = make_user(&fixture, "member@example.com", Some(&acme.org_id)).await?;
        let loner = make_user(&fixture, "loner@example.com", None).await?;

        let unowned = make_client(&fixture, "ev1_a", None).await?;
        let direct = make_client(
            &fixture,
            "ev1_b",
            Some(ClientOwner::Org {
                org_id: acme.org_id.clone(),
            }),
        )
        .await?;
        let via_member = make_client(
            &fixture,
            "ev1_c",
            Some(ClientOwner::User {
                user_id: member.user_id,
            }),
        )
        .await?;
        let via_loner = make_client(
            &fixture,
            "ev1_d",
            Some(ClientOwner::User {
                user_id: loner.user_id,
            }),
        )
        .await?;

        assert_eq!(owning_org(&*fixture.orgs, &unowned.client_id).await?, None);
        assert_eq!(
            owning_org(&*fixture.orgs, &direct.client_id).await?,
            Some(acme.org_id.clone())
        );
        assert_eq!(
            owning_org(&*fixture.orgs, &via_member.client_id).await?,
            Some(acme.org_id)
        );
        assert_eq!(
            owning_org(&*fixture.orgs, &via_loner.client_id).await?,
            None,
            "an owner belonging to no organization leaves the device in none"
        );
        Ok(())
    }

    /// A fleet scope reaches every device in both directions; an unaffiliated
    /// caller reaches none, whatever grant it holds.
    #[rstest]
    #[case::fleet(DeviceScope::Fleet, true)]
    #[case::unaffiliated(DeviceScope::Unaffiliated, false)]
    fn the_outer_scopes_are_total(#[case] scope: DeviceScope, #[case] expected: bool) {
        let other = OrgId::mint();
        assert_eq!(scope.may_administer(Some(&other)), expected);
        assert_eq!(scope.may_administer(None), expected);
        assert_eq!(scope.may_assign_to(Some(&other)), expected);
        assert_eq!(scope.may_assign_to(None), expected);
        assert_eq!(scope.may_hand_to(Some(&other)), expected);
        assert_eq!(scope.may_hand_to(None), expected);
    }

    #[test]
    fn an_org_scope_reaches_its_own_only() {
        let mine = OrgId::mint();
        let theirs = OrgId::mint();
        let scope = DeviceScope::Org(mine.clone());

        assert!(scope.may_administer(Some(&mine)), "its own device");
        assert!(
            !scope.may_administer(None),
            "an unowned device, or one an unaffiliated person owns, is the server's"
        );
        assert!(
            !scope.may_administer(Some(&theirs)),
            "another organization's device is never reachable"
        );

        assert!(
            scope.may_assign_to(Some(&mine)),
            "into its own organization"
        );
        assert!(scope.may_assign_to(None), "detaching is never a grab");
        assert!(
            !scope.may_assign_to(Some(&theirs)),
            "a device may not be handed to another organization"
        );

        assert!(scope.may_hand_to(Some(&mine)), "to one of its own members");
        assert!(
            !scope.may_hand_to(None),
            "a person outside every organization is not a detach target"
        );
        assert!(!scope.may_hand_to(Some(&theirs)));
    }

    /// Detaching leaves the device registered and unowned, where a fresh
    /// self-registration sits.
    #[tokio::test]
    async fn an_owner_is_a_reference_an_account_deletion_would_strand() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let owner = make_user(&fixture, "owner@example.com", Some(&acme.org_id)).await?;
        let laptop = make_client(
            &fixture,
            "ev1_laptop",
            Some(ClientOwner::User {
                user_id: owner.user_id.clone(),
            }),
        )
        .await?;
        // A device the organization owns directly is not this account's.
        make_client(
            &fixture,
            "ev1_rack",
            Some(ClientOwner::Org {
                org_id: acme.org_id.clone(),
            }),
        )
        .await?;

        assert_eq!(
            devices_owned_by(&*fixture.orgs, &owner.user_id).await?,
            vec![laptop.client_id.clone()]
        );

        assert_eq!(
            detach_devices_of(&*fixture.orgs, &owner.user_id).await?,
            vec![laptop.client_id.clone()]
        );
        assert_eq!(
            fixture.orgs.get_ownership(&laptop.client_id).await?,
            None,
            "the device survives its owner and becomes unowned"
        );
        assert!(
            devices_owned_by(&*fixture.orgs, &owner.user_id)
                .await?
                .is_empty()
        );
        // Idempotent, so a partially failed teardown is finished by re-running.
        assert!(
            detach_devices_of(&*fixture.orgs, &owner.user_id)
                .await?
                .is_empty()
        );
        Ok(())
    }

    /// It also counts a member's device against the member's organization, and a
    /// reference to a deleted organization nowhere.
    #[tokio::test]
    async fn the_tally_agrees_with_the_per_organization_joins() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let labs = make_org(&fixture, "labs").await?;
        let member = make_user(&fixture, "member@example.com", Some(&acme.org_id)).await?;
        make_user(&fixture, "loner@example.com", None).await?;
        // Names an organization that no longer exists.
        let ghost = OrgId::mint();
        make_user(&fixture, "ghost@example.com", Some(&ghost)).await?;

        make_client(
            &fixture,
            "ev1_rack",
            Some(ClientOwner::Org {
                org_id: acme.org_id.clone(),
            }),
        )
        .await?;
        make_client(
            &fixture,
            "ev1_laptop",
            Some(ClientOwner::User {
                user_id: member.user_id,
            }),
        )
        .await?;
        make_client(&fixture, "ev1_unowned", None).await?;

        let orgs = fixture.orgs.list_orgs().await?;
        let memberships = fixture.orgs.list_memberships().await?;
        let ownerships = fixture.orgs.list_ownerships().await?;
        let counts = tally(&orgs, &memberships, &ownerships);

        let acme_counts = counts
            .get(&acme.org_id)
            .copied()
            .ok_or_else(|| anyhow::anyhow!("acme is tallied"))?;
        assert_eq!(acme_counts.members, 1);
        assert_eq!(acme_counts.clients, 2, "the rack and the member's laptop");
        assert_eq!(
            counts.get(&labs.org_id).copied(),
            Some(OrgCounts::default())
        );
        assert!(
            !counts.contains_key(&ghost),
            "a reference to a deleted organization is counted nowhere"
        );

        // And it is the same answer the per-organization joins give.
        assert_eq!(
            acme_counts.members,
            members(&*fixture.orgs, &*fixture.users, &acme.org_id)
                .await?
                .len()
        );
        assert_eq!(
            acme_counts.clients,
            owned_clients(&*fixture.clients, &*fixture.orgs, &acme.org_id)
                .await?
                .len()
        );
        Ok(())
    }

    #[tokio::test]
    async fn detaching_clears_the_direct_references_only() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let member = make_user(&fixture, "member@example.com", Some(&acme.org_id)).await?;
        let rack = make_client(
            &fixture,
            "ev1_rack",
            Some(ClientOwner::Org {
                org_id: acme.org_id.clone(),
            }),
        )
        .await?;
        let laptop = make_client(
            &fixture,
            "ev1_laptop",
            Some(ClientOwner::User {
                user_id: member.user_id.clone(),
            }),
        )
        .await?;

        let found = fixture.dependents(&acme.org_id).await?;
        assert_eq!(found.members, vec![member.user_id.clone()]);
        assert_eq!(
            found.clients,
            vec![rack.client_id.clone()],
            "a member's own device is not a direct reference"
        );
        assert!(!found.is_empty());

        detach_all(&*fixture.orgs, &acme.org_id).await?;
        assert!(fixture.dependents(&acme.org_id).await?.is_empty());
        assert_eq!(fixture.orgs.get_membership(&member.user_id).await?, None);
        assert_eq!(fixture.orgs.get_ownership(&rack.client_id).await?, None);
        assert_eq!(
            fixture.orgs.get_ownership(&laptop.client_id).await?,
            Some(ClientOwner::User {
                user_id: member.user_id
            }),
            "the member's device keeps its owner; the membership is what moved"
        );

        // Idempotent: a second pass is a no-op, not an error.
        assert!(detach_all(&*fixture.orgs, &acme.org_id).await?.is_empty());
        Ok(())
    }

    /// Detaching keeps them: they are its entries, not pointers at it.
    #[tokio::test]
    async fn private_benchmarks_are_dependents_detaching_keeps() -> anyhow::Result<()> {
        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let labs = make_org(&fixture, "labs").await?;
        for (org, id) in [
            (&acme, "prefill_throughput_acme"),
            (&labs, "prefill_throughput_labs"),
        ] {
            let entry = Benchmark::from_def(BenchmarkDefinition::PrefillThroughput(
                PrefillThroughputBenchmark {
                    benchmark_id: BenchmarkId::try_new(id)?,
                    parameter_prefill_tokens: pipette_plan_types::NonNegativeCount::try_new(64)?,
                    parameter_batch_size: pipette_plan_types::BatchSize::one(),
                },
            ))?;
            fixture
                .catalog
                .put_org_benchmark(&org.org_id, &entry)
                .await?;
        }

        let found = fixture.dependents(&acme.org_id).await?;
        assert_eq!(
            found.benchmarks,
            vec![BenchmarkId::try_new("prefill_throughput_acme")?]
        );
        assert!(found.members.is_empty() && found.clients.is_empty());
        assert!(!found.is_empty());
        assert!(found.describe().contains("1 private benchmark(s)"));

        assert!(detach_all(&*fixture.orgs, &acme.org_id).await?.is_empty());
        assert_eq!(
            fixture.dependents(&acme.org_id).await?.benchmarks,
            found.benchmarks
        );
        Ok(())
    }

    /// An expired pre-auth key registers nothing, so it is not counted.
    #[tokio::test]
    async fn owned_campaigns_vault_records_and_live_preauth_keys_are_dependents()
    -> anyhow::Result<()> {
        use pipette_core::campaign::NewCampaign;
        use pipette_core::client_preauth::{ClientPreauthParams, mint};
        use pipette_plan_types::{ClientPreauthUsage, PlanId};

        let fixture = fixture()?;
        let acme = make_org(&fixture, "acme").await?;
        let labs = make_org(&fixture, "labs").await?;
        let as_owner = |org: &Organization| PrincipalOwner::Org {
            org_id: org.org_id.clone(),
        };
        let mut held = Holdings::default();
        let mut keys = Vec::new();
        for org in [&acme, &labs] {
            let campaign =
                NewCampaign::new(NonEmptyTrimmedString::try_new("nightly")?, as_owner(org))
                    .around(PlanId::from_uuid(uuid::Uuid::now_v7()), Utc::now());
            fixture.campaigns.put_campaign(&campaign).await?;
            let resource = pipette_store_testkit::vault_resource_for(
                as_owner(org),
                pipette_store_testkit::vault_blob_sha256()?,
                1,
            )?;
            fixture.vault.put_resource(&resource).await?;
            for expires_in in [1, -1] {
                let key = mint(
                    ClientPreauthParams {
                        usage: ClientPreauthUsage::MultiUse,
                        expires_at: Some(Utc::now() + chrono::Duration::days(expires_in)),
                        default_tags: Default::default(),
                        default_organization: None,
                        default_owner: Some(ClientOwner::Org {
                            org_id: org.org_id.clone(),
                        }),
                        default_permissions: None,
                        note: None,
                    },
                    Utc::now(),
                )?
                .key;
                fixture.clients.put_client_preauth_key(&key).await?;
                if org.org_id == acme.org_id && expires_in > 0 {
                    keys.push(key.key_id);
                }
            }
            if org.org_id == acme.org_id {
                held.campaigns.push(campaign.campaign_id);
                held.vault_resources.push(resource.resource_id);
            }
        }

        let found = fixture.dependents(&acme.org_id).await?;
        assert_eq!(found.held, held);
        assert_eq!(found.preauth_keys, keys);
        assert!(!found.only_detachable());
        assert_eq!(
            found.describe(),
            "1 campaign(s), 1 vault resource(s) and 1 client pre-auth key(s) still reference it"
        );
        assert!(detach_all(&*fixture.orgs, &acme.org_id).await?.is_empty());
        assert_eq!(fixture.dependents(&acme.org_id).await?, found);

        // Each kind refuses on its own.
        let alone = [
            Dependents {
                held: Holdings {
                    campaigns: found.held.campaigns.clone(),
                    ..Holdings::default()
                },
                ..Dependents::default()
            },
            Dependents {
                held: Holdings {
                    vault_resources: found.held.vault_resources.clone(),
                    ..Holdings::default()
                },
                ..Dependents::default()
            },
            Dependents {
                preauth_keys: found.preauth_keys.clone(),
                ..Dependents::default()
            },
        ];
        assert!(
            alone
                .iter()
                .all(|kind| !kind.only_detachable() && !kind.is_empty())
        );
        assert_eq!(
            alone.iter().map(Dependents::describe).collect::<Vec<_>>(),
            [
                "1 campaign(s) still reference it",
                "1 vault resource(s) still reference it",
                "1 client pre-auth key(s) still reference it",
            ]
        );
        Ok(())
    }
}
