//! Who may read a vault resource, who may administer one, and which devices may
//! fetch its bytes. No permission names of its own: your own vault needs no
//! grant, and an organization's follows the campaign rule.

use std::collections::BTreeSet;

use pipette_core::user::User;
use pipette_core::vault::{VaultOwner, VaultResource};
use pipette_pipeline::org::owning_org;
use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserId, UserPermission, VaultPrincipalId};

use crate::auth::{Caller, UserRights, user_org};
use crate::error::AppError;
use crate::owner::HandOffScope;
use crate::state::AppState;

/// Which vault resources a caller may **read**. Never refuses: every active
/// account reaches its own.
pub enum VaultReach {
    /// `manage_any_orgs`: every resource. Not `read_any_orgs`, the roster read.
    Any,
    /// The caller's own, plus its organization's when it belongs to one.
    Own { user: UserId, org: Option<OrgId> },
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
            user: user.user_id.clone(),
            // Membership alone: a member reads the team's artifacts and does
            // not administer them, as with a campaign's record.
            org: user_org(state, &user.user_id).await?,
        })
    }

    pub fn admits(&self, owner: &VaultOwner) -> bool {
        match (self, owner) {
            (Self::Any, _) => true,
            (Self::Own { user, .. }, VaultOwner::User { user_id }) => user_id == user,
            (Self::Own { org, .. }, VaultOwner::Org { org_id }) => {
                org.as_ref().is_some_and(|mine| mine == org_id)
            }
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
    /// Its own resources are always its own, hence no `own` flag.
    user_id: UserId,
    /// `manage_any_orgs`: every resource, whoever owns it.
    server: bool,
    /// `manage_orgs` **and** membership; `None` without a membership.
    admin_of: Option<OrgId>,
    /// Membership, grant or no grant: the organization it may upload **for**.
    member_of: Option<OrgId>,
}

impl HandOffScope for VaultAuthority {
    fn user_id(&self) -> &UserId {
        &self.user_id
    }
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
    /// Resolve the authority. **Never refuses**: every active account
    /// administers its own vault, so there is no set that reaches nothing.
    pub async fn resolve(state: &AppState, caller: Caller<'_>) -> Result<Self, AppError> {
        let user = caller.user;
        let rights = caller.rights;
        let member_of = user_org(state, &user.user_id).await?;
        Ok(Self {
            user_id: user.user_id.clone(),
            server: rights.holds(UserPermission::ManageAnyOrgs),
            admin_of: rights
                .holds(UserPermission::ManageOrgs)
                .then(|| member_of.clone())
                .flatten(),
            member_of,
        })
    }

    /// Whether this caller may delete a resource owned by `owner`. Uploading is
    /// [`HandOffScope::may_hand_to`], which a member passes for its organization.
    pub fn may_administer(&self, owner: &VaultOwner) -> bool {
        if self.server {
            return true;
        }
        match owner {
            VaultOwner::User { user_id } => *user_id == self.user_id,
            VaultOwner::Org { org_id } => self.admin_of.as_ref() == Some(org_id),
        }
    }

    /// Whether this caller may set the parameter counts of a model owned by
    /// `owner`: an administrator's, so a personal file's stay the server's
    /// where its delete is its owner's.
    pub fn may_set_model_params(&self, owner: &VaultOwner) -> bool {
        self.server
            || matches!(owner, VaultOwner::Org { org_id } if self.admin_of.as_ref() == Some(org_id))
    }

    /// The owner a resource is registered under when the request names none.
    pub fn own_owner(&self) -> VaultOwner {
        VaultOwner::User {
            user_id: self.user_id.clone(),
        }
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
        Err(AppError::Forbidden(self.refusal(&resource.owner).into()))
    }

    /// Why a check refused `owner`: a member reads its organization's
    /// resources, so it is told the grant it lacks rather than "another owner".
    fn refusal(&self, owner: &VaultOwner) -> &'static str {
        match owner {
            VaultOwner::Org { org_id } if self.member_of.as_ref() == Some(org_id) => {
                "deleting your organization's vault resources needs manage_orgs"
            }
            VaultOwner::Org { .. } | VaultOwner::User { .. } => {
                "this vault resource belongs to another owner"
            }
        }
    }
}

/// Whether `client` may fetch the bytes of a resource owned by `owner`: what
/// its owner owns, never by user permission. A personal artifact stays personal
/// when its owner joins a team; an unowned device reaches nothing.
pub async fn client_may_fetch(
    state: &AppState,
    client_id: &ClientId,
    owner: &VaultOwner,
) -> Result<bool, AppError> {
    Ok(match owner {
        VaultOwner::User { user_id } => {
            let device_owner = state
                .org_store
                .get_ownership(client_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read the device's owner")))?;
            matches!(device_owner, Some(ClientOwner::User { user_id: owner_id }) if &owner_id == user_id)
        }
        VaultOwner::Org { org_id } => {
            owning_org(state.org_store.as_ref(), client_id)
                .await
                .map_err(|e| {
                    AppError::internal(e.context("failed to resolve the device's organization"))
                })?
                .as_ref()
                == Some(org_id)
        }
    })
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

    fn user(id: &UserId) -> VaultOwner {
        VaultOwner::User {
            user_id: id.clone(),
        }
    }

    /// The three rules, as a table against the owners they are not about.
    #[rstest]
    // Holding nothing still owns its own and reaches nothing else; `resolve`
    // gives a `manage_orgs` holder on no organization `admin_of: None` too.
    #[case::plain(false, false, true, false, false)]
    // An org admin administers the team's, and its own alongside.
    #[case::org_admin(true, false, true, true, false)]
    #[case::server(false, true, true, true, true)]
    fn the_authority_rules_hold(
        #[case] admin: bool,
        #[case] server: bool,
        #[case] reaches_own: bool,
        #[case] reaches_org: bool,
        #[case] reaches_stranger: bool,
    ) {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let authority = VaultAuthority {
            user_id: me.clone(),
            server,
            admin_of: admin.then(|| my_org.clone()),
            member_of: Some(my_org.clone()),
        };

        assert_eq!(authority.may_administer(&user(&me)), reaches_own);
        assert_eq!(authority.may_administer(&org(&my_org)), reaches_org);
        assert_eq!(
            authority.may_administer(&user(&UserId::mint())),
            reaches_stranger,
            "another account's artifact"
        );
        assert_eq!(
            authority.may_administer(&org(&OrgId::mint())),
            reaches_stranger,
            "another organization's artifact"
        );
    }

    #[test]
    fn a_refusal_names_the_grant_a_member_lacks() {
        let my_org = OrgId::mint();
        let member = VaultAuthority {
            user_id: UserId::mint(),
            server: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(member.refusal(&org(&my_org)).contains("needs manage_orgs"));
        for stranger in [org(&OrgId::mint()), user(&UserId::mint())] {
            assert_eq!(
                member.refusal(&stranger),
                "this vault resource belongs to another owner"
            );
        }
    }

    #[test]
    fn a_member_uploads_for_the_team_and_no_colleague() {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let member = VaultAuthority {
            user_id: me.clone(),
            server: false,
            admin_of: None,
            member_of: Some(my_org.clone()),
        };
        assert!(member.may_hand_to(&org(&my_org), Some(&my_org)));
        assert!(member.may_hand_to(&user(&me), Some(&my_org)));
        assert!(!member.may_hand_to(&user(&UserId::mint()), Some(&my_org)));
        let other = OrgId::mint();
        assert!(!member.may_hand_to(&org(&other), Some(&other)));
        assert!(!member.may_administer(&org(&my_org)));
    }

    #[test]
    fn a_member_reads_the_team_s_and_no_colleague_s() {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let member = VaultReach::Own {
            user: me.clone(),
            org: Some(my_org.clone()),
        };
        assert!(member.admits(&user(&me)));
        assert!(member.admits(&org(&my_org)));
        assert!(!member.admits(&user(&UserId::mint())));
        assert!(!member.admits(&org(&OrgId::mint())));

        let unaffiliated = VaultReach::Own {
            user: me,
            org: None,
        };
        assert!(!unaffiliated.admits(&org(&my_org)));
        assert!(VaultReach::Any.admits(&org(&OrgId::mint())));
    }
}
