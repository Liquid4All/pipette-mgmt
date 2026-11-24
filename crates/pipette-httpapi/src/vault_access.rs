//! Who may read a vault resource, who may administer one, and which devices may
//! fetch its bytes. No permission names of its own: an organization's vault
//! follows the campaign rule.

use std::collections::BTreeSet;

use pipette_core::user::User;
use pipette_core::vault::{VaultOwner, VaultResource};
use pipette_pipeline::org::owning_org;
use pipette_plan_types::{ClientId, OrgId, UserPermission, VaultPrincipalId};

use crate::auth::{Caller, UserRights, user_org};
use crate::campaign_access::NO_ORGANIZATION;
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

/// Which vault resources a caller may **read**. Never refuses: an account in
/// no organization reaches nothing.
pub enum VaultReach {
    /// `manage_any_orgs`: every resource. Not `read_any_orgs`, the roster read.
    Any,
    /// Its organization's, when it belongs to one.
    Own { org: Option<OrgId> },
}

impl VaultReach {
    /// Resolve the reach from the caller's permission set, **once per request**,
    /// so no row is judged against a set that moved underneath a listing.
    pub async fn resolve(state: &AppState, caller: Caller<'_>) -> Result<Self, AppError> {
        let user = caller.user;
        let rights = caller.rights;
        Self::from_rights(state, rights, user).await
    }

    /// The same decision from a set already loaded — what the content route
    /// uses, having taken its rights through `VaultFetcher`.
    pub async fn from_rights(
        state: &AppState,
        rights: &UserRights,
        user: &User,
    ) -> Result<Self, AppError> {
        if rights.holds(UserPermission::ManageAnyOrgs) {
            return Ok(Self::Any);
        }
        Ok(Self::Own {
            // Membership alone: a member reads the team's artifacts and does
            // not administer them, as with a campaign's record.
            org: user_org(state, &user.user_id).await?,
        })
    }

    pub fn admits(&self, owner: &VaultOwner) -> bool {
        match self {
            Self::Any => true,
            Self::Own { org } => org.as_ref() == Some(owner.org_id()),
        }
    }

    /// Refuse unless this reach admits `resource`. A `403` rather than a `404`,
    /// for [`VaultAuthority::require`]'s reason.
    pub fn require(&self, resource: &VaultResource) -> Result<(), AppError> {
        if self.admits(&resource.owner) {
            return Ok(());
        }
        tracing::warn!(
            resource_id = %resource.resource_id,
            "refused: the vault resource is outside the caller's reach"
        );
        Err(AppError::Forbidden(
            "this vault resource belongs to another owner".into(),
        ))
    }
}

/// How far a caller's authority to **administer** vault resources reaches, as
/// independent facts like [`crate::campaign_access::CampaignAuthority`].
#[derive(Clone)]
pub struct VaultAuthority {
    /// `manage_any_orgs`: every resource, whoever owns it.
    server: bool,
    /// `manage_orgs` **and** membership; `None` without a membership.
    admin_of: Option<OrgId>,
    /// Membership, grant or no grant: the organization it may upload **for**.
    member_of: Option<OrgId>,
}

impl HandOffScope for VaultAuthority {
    fn is_server(&self) -> bool {
        self.server
    }
    fn admin_of(&self) -> Option<&OrgId> {
        self.admin_of.as_ref()
    }
    fn gives_to(&self) -> Option<&OrgId> {
        self.member_of.as_ref()
    }
}

impl VaultAuthority {
    /// Resolve the authority. **Never refuses**: an account that reaches
    /// nothing is refused record by record.
    pub async fn resolve(state: &AppState, caller: Caller<'_>) -> Result<Self, AppError> {
        let user = caller.user;
        let rights = caller.rights;
        let member_of = user_org(state, &user.user_id).await?;
        Ok(Self {
            server: rights.holds(UserPermission::ManageAnyOrgs),
            admin_of: rights
                .holds(UserPermission::ManageOrgs)
                .then(|| member_of.clone())
                .flatten(),
            member_of,
        })
    }

    /// Whether this caller may delete a resource owned by `owner`: any member
    /// of its organization, as it deleted the ones it owned itself. Uploading
    /// is [`HandOffScope::may_hand_to`], which a member passes too.
    pub fn may_administer(&self, owner: &VaultOwner) -> bool {
        self.server || self.member_of.as_ref() == Some(owner.org_id())
    }

    /// Whether this caller may set the parameter counts of a model owned by
    /// `owner`: an administrator's call, as before.
    pub fn may_set_model_params(&self, owner: &VaultOwner) -> bool {
        self.server || self.admin_of.as_ref() == Some(owner.org_id())
    }

    /// The owner a resource is registered under when the request names none:
    /// the caller's organization, or a `400` for a caller in none.
    pub fn own_owner(&self) -> Result<VaultOwner, AppError> {
        self.member_of
            .clone()
            .map(|org_id| VaultOwner::Org { org_id })
            .ok_or_else(|| AppError::BadRequest(NO_ORGANIZATION.into()))
    }

    /// A `403`, not a `404`: a resource id is a minted UUIDv7, so the true
    /// answer discloses nothing.
    pub fn require(&self, resource: &VaultResource) -> Result<(), AppError> {
        if self.may_administer(&resource.owner) {
            return Ok(());
        }
        tracing::warn!(
            resource_id = %resource.resource_id,
            owner = %resource.owner.label(),
            "refused: the vault resource is outside the caller's scope"
        );
        Err(AppError::Forbidden(
            "this vault resource belongs to another owner".into(),
        ))
    }
}

/// Whether `client` may fetch the bytes of a resource owned by `owner`: what
/// its owner owns, never by user permission. An unowned device reaches nothing.
pub async fn client_may_fetch(
    state: &AppState,
    client_id: &ClientId,
    owner: &VaultOwner,
) -> Result<bool, AppError> {
    Ok(owning_org(state.org_store.as_ref(), client_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to resolve the device's organization")))?
        .as_ref()
        == Some(owner.org_id()))
}

/// Whether `client_id` may fetch every vault resource a job body names. A
/// malformed vault reference fails closed.
pub async fn client_may_fetch_job(
    state: &AppState,
    client_id: &ClientId,
    body: &serde_json::Value,
) -> Result<bool, AppError> {
    let principals: BTreeSet<String> = pipette_pipeline::vault_check::vault_refs(body)
        .map_err(|e| AppError::Internal(format!("the job carries a malformed vault source: {e}")))?
        .into_iter()
        .map(|vault_ref| vault_ref.principal)
        .collect();
    let owners = principals
        .iter()
        .map(|principal| {
            principal
                .parse::<VaultPrincipalId>()
                .map(VaultOwner::from)
                .map_err(|e| {
                    AppError::Internal(format!("the job carries a malformed vault principal: {e}"))
                })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let fetchable = futures::future::try_join_all(
        owners
            .iter()
            .map(|owner| client_may_fetch(state, client_id, owner)),
    )
    .await?;
    Ok(fetchable.into_iter().all(|may| may))
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_core::types::Mint;

    use super::*;

    fn org(id: &OrgId) -> VaultOwner {
        VaultOwner::Org { org_id: id.clone() }
    }

    /// The rules, as a table against the owners they are not about: a member
    /// deletes its organization's resources, and only an administrator sets
    /// their parameter counts.
    #[rstest]
    #[case::member(false, false, false, false)]
    #[case::org_admin(true, false, true, false)]
    #[case::server(false, true, true, true)]
    fn the_authority_rules_hold(
        #[case] admin: bool,
        #[case] server: bool,
        #[case] sets_org_params: bool,
        #[case] reaches_stranger: bool,
    ) {
        let my_org = OrgId::mint();
        let stranger = org(&OrgId::mint());
        let authority = VaultAuthority {
            server,
            admin_of: admin.then(|| my_org.clone()),
            member_of: Some(my_org.clone()),
        };

        assert!(authority.may_administer(&org(&my_org)));
        assert_eq!(
            authority.may_set_model_params(&org(&my_org)),
            sets_org_params
        );
        assert_eq!(
            authority.may_administer(&stranger),
            reaches_stranger,
            "another organization's artifact"
        );
        assert_eq!(authority.may_set_model_params(&stranger), reaches_stranger);
    }

    #[test]
    fn a_member_uploads_for_the_team_alone() {
        let my_org = OrgId::mint();
        let member = VaultAuthority {
            server: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(member.may_hand_to(&org(&my_org)));
        assert!(!member.may_hand_to(&org(&OrgId::mint())));
    }

    #[test]
    fn a_member_reads_the_team_s_alone() {
        let my_org = OrgId::mint();
        let member = VaultReach::Own {
            org: Some(my_org.clone()),
        };
        assert!(member.admits(&org(&my_org)));
        assert!(!member.admits(&org(&OrgId::mint())));

        let unaffiliated = VaultReach::Own { org: None };
        assert!(!unaffiliated.admits(&org(&my_org)));
        assert!(VaultReach::Any.admits(&org(&OrgId::mint())));
    }
}
