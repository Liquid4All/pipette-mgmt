//! The choices a device's and a campaign's owner fields offer, and the
//! `.../owners` lookups over them. A record inside an organization stays inside
//! it, and no lookup names whom the roster grant does not read.

use std::collections::HashSet;

use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};

use pipette_core::organization::Organization;
use pipette_core::user::User;
use pipette_pipeline::org::{DeviceScope, RosterScope};
use pipette_plan_types::{
    CampaignId, ClientId, OrgId, OwnerMatch, OwnerMatches, OwnerType, PrincipalOwner, UserId,
    UserPermission, VaultPrincipalId,
};
use pipette_store::STORAGE_CONCURRENCY;

use crate::api_v1::orgs::orgs_in;
use crate::auth::{Caller, UserRights};
use crate::campaign_access::CampaignAuthority;
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

/// The destinations a form offers, organizations first, then accounts by
/// address.
#[derive(Default)]
pub(crate) struct OwnerChoices(pub(crate) Vec<OwnerMatch>);

/// How many owners a lookup answers: an autocomplete, not a listing.
const MATCHES_MAX: usize = 20;

impl OwnerChoices {
    fn of(
        orgs: &[Organization],
        accounts: &[(User, Option<OrgId>)],
        named: &[Organization],
    ) -> Self {
        let orgs = orgs.iter().map(|org| OwnerMatch {
            value: VaultPrincipalId::Org(org.org_id.clone()),
            name: org.slug.to_string(),
            label: org.slug.to_string(),
            owner_type: OwnerType::Org,
        });
        let accounts = accounts.iter().map(|(account, org_id)| {
            let on = org_id
                .as_ref()
                .and_then(|id| named.iter().find(|org| org.org_id == *id));
            OwnerMatch {
                value: VaultPrincipalId::User(account.user_id.clone()),
                name: account.contact_email.to_string(),
                label: match on {
                    Some(org) => format!("{} ({})", account.contact_email, org.slug),
                    None => account.contact_email.to_string(),
                },
                owner_type: OwnerType::User,
            }
        });
        Self(orgs.chain(accounts).collect())
    }

    /// The first [`MATCHES_MAX`] whose handle, address or id holds `query`,
    /// case aside, narrowed to `owner_type` when one is given. The one `query`
    /// names exactly comes first, so typed text resolves whatever the cut leaves
    /// out.
    pub(crate) fn matching(&self, query: &str, owner_type: Option<OwnerType>) -> OwnerMatches {
        let query = query.trim().to_lowercase();
        let fields = |c: &OwnerMatch| {
            [
                c.name.to_lowercase(),
                c.label.to_lowercase(),
                c.value.to_string().to_lowercase(),
            ]
        };
        let mut held: Vec<&OwnerMatch> = self
            .0
            .iter()
            .filter(|c| owner_type.is_none_or(|wanted| c.owner_type == wanted))
            .filter(|c| fields(c).iter().any(|field| field.contains(&query)))
            .collect();
        if let Some(at) = held.iter().position(|c| fields(c).contains(&query)) {
            let exact = held.remove(at);
            held.insert(0, exact);
        }
        OwnerMatches {
            owners: held.into_iter().take(MATCHES_MAX).cloned().collect(),
        }
    }
}

/// Where `user` may hand the campaign `campaign_id` names. Read first, so one
/// outside the reach is refused as its read is, then the authority over its
/// record, which a disabled member's administrator holds too.
pub(crate) async fn campaign_choices(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<OwnerChoices, AppError> {
    let campaign = crate::api_v1::campaigns::reads::for_read(state, caller, campaign_id).await?;
    CampaignAuthority::resolve(state, caller)
        .await?
        .require_record(state, caller.rights, &campaign)
        .await?;
    for_campaign(state, caller, &campaign.owner).await
}

/// Where `user` may hand the device `client_id` names. Read first, so an
/// unknown device is a `404` and the fleet grant applies, then the device scope.
pub(crate) async fn device_choices(
    state: &AppState,
    caller: Caller<'_>,
    client_id: &ClientId,
) -> Result<OwnerChoices, AppError> {
    crate::api_v1::clients::reads::one(state, client_id).await?;
    let owning = pipette_pipeline::org::owning_org(&*state.org_store, client_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to resolve the owner")))?;
    device_owners(state, caller, owning.as_ref())
        .await?
        .ok_or_else(|| AppError::Forbidden("this device is outside your device scope".into()))
}

/// Where `user` may hand a device that rolls up to `owning`, or `None` when its
/// device scope does not reach the device.
async fn device_owners(
    state: &AppState,
    caller: Caller<'_>,
    owning: Option<&OrgId>,
) -> Result<Option<OwnerChoices>, AppError> {
    match crate::api_v1::clients::device_reach(state, caller).await? {
        Some(scope) if scope.may_administer(owning) => {
            let read = crate::api_v1::users::read_reach(state, caller).await?;
            Ok(Some(
                for_device(state, caller.user, &scope, &read, owning).await?,
            ))
        }
        _ => Ok(None),
    }
}

/// Where a device that rolls up to `boundary` may be put: inside it, or for one
/// in none, wherever `scope` reaches, among whom the roster grant `read` reads;
/// a scoped caller is offered only itself among accounts, since a person's
/// device fetches their vault. Moving between organizations is detach, then claim.
async fn for_device(
    state: &AppState,
    user: &User,
    scope: &DeviceScope,
    read: &RosterScope,
    boundary: Option<&OrgId>,
) -> Result<OwnerChoices, AppError> {
    let within = match (boundary, scope) {
        (Some(org_id), _) => RosterScope::Org(org_id.clone()),
        (None, DeviceScope::Fleet) => RosterScope::Fleet,
        // A device in no organization is the fleet scope's alone.
        (None, DeviceScope::Org(_) | DeviceScope::Unaffiliated) => {
            return Ok(OwnerChoices::default());
        }
    };
    let (mut orgs, mut accounts) = futures::try_join!(
        orgs_in(state, &within),
        crate::api_v1::users::roster(state, &within),
    )?;
    orgs.retain(|org| read.covers(Some(&org.org_id)));
    accounts.retain(|(account, org_id)| {
        read.covers(org_id.as_ref())
            && (*scope == DeviceScope::Fleet || account.user_id == user.user_id)
    });
    Ok(OwnerChoices::of(&orgs, &sorted(accounts), &orgs))
}

/// Where `user` may hand a campaign owned by `current`: inside its organization,
/// by [`HandOffScope::may_hand_to`], less the current owner and accounts the route
/// refuses for lacking `submit_plans`.
async fn for_campaign(
    state: &AppState,
    caller: Caller<'_>,
    current: &PrincipalOwner,
) -> Result<OwnerChoices, AppError> {
    let user = caller.user;
    let (authority, read, boundary) = futures::try_join!(
        CampaignAuthority::resolve(state, caller),
        crate::api_v1::users::read_reach(state, caller),
        boundary_of(state, current),
    )?;
    let scope_of = |org_id: Option<&OrgId>| {
        org_id.map_or(RosterScope::Unaffiliated, |org_id| {
            RosterScope::Org(org_id.clone())
        })
    };
    let (given, roster) = match &boundary {
        Some(org_id) => (scope_of(Some(org_id)), scope_of(Some(org_id))),
        None if authority.is_server() => (RosterScope::Fleet, RosterScope::Fleet),
        // Its own organization, and the members of the one it administers.
        None => (
            scope_of(authority.gives_to()),
            scope_of(authority.admin_of()),
        ),
    };
    let (mut orgs, mut accounts) = futures::try_join!(
        orgs_in(state, &given),
        crate::api_v1::users::roster(state, &roster),
    )?;
    let own_org = authority.gives_to();
    let is_self_inside = boundary.is_none() || boundary.as_ref() == own_org;
    if is_self_inside
        && !accounts
            .iter()
            .any(|(account, _)| account.user_id == user.user_id)
    {
        accounts.push((user.clone(), own_org.cloned()));
    }
    // Named before the organizations are narrowed, so an account's label keeps
    // the one it is on.
    let named = orgs.clone();

    let admitted = |owner: &PrincipalOwner, on: Option<&OrgId>| {
        owner != current && authority.may_hand_to(owner, on)
    };
    orgs.retain(|org| {
        (Some(&org.org_id) == own_org || read.covers(Some(&org.org_id)))
            && admitted(
                &PrincipalOwner::Org {
                    org_id: org.org_id.clone(),
                },
                Some(&org.org_id),
            )
    });
    accounts.retain(|(account, org_id)| {
        (account.user_id == user.user_id || read.covers(org_id.as_ref()))
            && admitted(
                &PrincipalOwner::User {
                    user_id: account.user_id.clone(),
                },
                org_id.as_ref(),
            )
    });
    let files = submitters(state, &accounts).await?;
    accounts.retain(|(account, _)| files.contains(&account.user_id));

    Ok(OwnerChoices::of(&orgs, &sorted(accounts), &named))
}

/// The organization a record owned by `owner` sits inside: the organization
/// itself, or the one the owning account belongs to.
pub(crate) async fn boundary_of(
    state: &AppState,
    owner: &PrincipalOwner,
) -> Result<Option<OrgId>, AppError> {
    match owner {
        PrincipalOwner::Org { org_id } => Ok(Some(org_id.clone())),
        PrincipalOwner::User { user_id } => crate::auth::user_org(state, user_id).await,
    }
}

/// Which of `accounts` hold `submit_plans`, read concurrently.
async fn submitters(
    state: &AppState,
    accounts: &[(User, Option<OrgId>)],
) -> Result<HashSet<UserId>, AppError> {
    let owned: Vec<User> = accounts.iter().map(|(a, _)| a.clone()).collect();
    let held: Vec<Option<UserId>> = stream::iter(owned.into_iter().map(|account| async move {
        let rights = UserRights::of_account(state, &account).await?;
        Ok::<_, AppError>(
            rights
                .holds(UserPermission::SubmitPlans)
                .then_some(account.user_id),
        )
    }))
    .buffer_unordered(STORAGE_CONCURRENCY)
    .try_collect()
    .await?;
    Ok(held.into_iter().flatten().collect())
}

fn sorted(mut accounts: Vec<(User, Option<OrgId>)>) -> Vec<(User, Option<OrgId>)> {
    accounts.sort_by(|(a, _), (b, _)| a.contact_email.as_str().cmp(b.contact_email.as_str()));
    accounts
}
