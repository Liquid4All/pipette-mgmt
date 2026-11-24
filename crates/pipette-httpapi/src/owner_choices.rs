//! The choices a device's and a campaign's owner fields offer, and the
//! `.../owners` lookups over them. A record inside an organization stays inside
//! it, and no lookup names whom the roster grant does not read.

use pipette_core::organization::Organization;
use pipette_pipeline::org::{DeviceScope, RosterScope};
use pipette_plan_types::{
    CampaignId, ClientId, OrgId, OwnerMatch, OwnerMatches, OwnerType, PrincipalOwner,
    VaultPrincipalId,
};

use crate::api_v1::orgs::orgs_in;
use crate::auth::Caller;
use crate::campaign_access::CampaignAuthority;
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

/// The organizations a form offers, by handle.
#[derive(Default)]
pub(crate) struct OwnerChoices(pub(crate) Vec<OwnerMatch>);

/// How many owners a lookup answers: an autocomplete, not a listing.
const MATCHES_MAX: usize = 20;

impl OwnerChoices {
    fn of(orgs: &[Organization]) -> Self {
        Self(
            orgs.iter()
                .map(|org| OwnerMatch {
                    value: VaultPrincipalId::Org(org.org_id.clone()),
                    name: org.slug.to_string(),
                    label: org.slug.to_string(),
                    owner_type: OwnerType::Org,
                })
                .collect(),
        )
    }

    /// The first [`MATCHES_MAX`] whose handle or id holds `query`,
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
/// record.
pub(crate) async fn campaign_choices(
    state: &AppState,
    caller: Caller<'_>,
    campaign_id: &CampaignId,
) -> Result<OwnerChoices, AppError> {
    let campaign = crate::api_v1::campaigns::reads::for_read(state, caller, campaign_id).await?;
    CampaignAuthority::resolve(state, caller)
        .await?
        .require(&campaign)?;
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

/// Where `user` may hand a device `owning` owns, or `None` when its device
/// scope does not reach the device.
async fn device_owners(
    state: &AppState,
    caller: Caller<'_>,
    owning: Option<&OrgId>,
) -> Result<Option<OwnerChoices>, AppError> {
    match crate::api_v1::clients::device_reach(state, caller).await? {
        Some(scope) if scope.may_administer(owning) => {
            let read = crate::api_v1::users::read_reach(state, caller).await?;
            Ok(Some(for_device(state, &scope, &read, owning).await?))
        }
        _ => Ok(None),
    }
}

/// Where a device `boundary` owns may be put: inside it, or for one in none,
/// wherever `scope` reaches, among the organizations the roster grant `read`
/// reads. Moving between organizations is detach, then claim.
async fn for_device(
    state: &AppState,
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
    let mut orgs = orgs_in(state, &within).await?;
    orgs.retain(|org| read.covers(Some(&org.org_id)));
    Ok(OwnerChoices::of(&orgs))
}

/// Where `user` may hand a campaign owned by `current`: inside its
/// organization, by [`HandOffScope::may_hand_to`], less the current owner.
async fn for_campaign(
    state: &AppState,
    caller: Caller<'_>,
    current: &PrincipalOwner,
) -> Result<OwnerChoices, AppError> {
    let (authority, read) = futures::try_join!(
        CampaignAuthority::resolve(state, caller),
        crate::api_v1::users::read_reach(state, caller),
    )?;
    let mut orgs = orgs_in(state, &RosterScope::Org(current.org_id().clone())).await?;
    let own_org = authority.gives_to();
    orgs.retain(|org| {
        let owner = PrincipalOwner::Org {
            org_id: org.org_id.clone(),
        };
        &owner != current
            && (Some(&org.org_id) == own_org || read.covers(Some(&org.org_id)))
            && authority.may_hand_to(&owner)
    });
    Ok(OwnerChoices::of(&orgs))
}
