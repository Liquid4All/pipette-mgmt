//! Where an owned record may be **put down**, and which slice of a listing a
//! caller asked for, shared by the campaign and vault routes so the two cannot
//! drift.

use std::num::NonZeroUsize;

use pipette_core::campaign::{Campaign, CampaignGroup};
use pipette_core::config::Config;
use pipette_core::vault::VaultResource;
use pipette_plan_types::{
    CampaignGroupId, CampaignId, ListOrder, ListPage, OrgId, OwnedListQuery, PrincipalOwner,
    VaultResourceId,
};
use pipette_store::Page;

use crate::error::AppError;
use crate::list::{list_cursor, list_limit, page_filtered, page_newest};
use crate::state::AppState;

/// How far a caller may reach when it hands a record to somebody: three facts,
/// not one scope, since a caller can hold several at once.
pub trait HandOffScope {
    /// Whether this caller reaches every record, whoever owns it.
    fn is_server(&self) -> bool;
    /// The organization this caller administers: the grant **and** membership.
    fn admin_of(&self) -> Option<&OrgId>;
    /// The organization this caller may put a record down on: its membership,
    /// for a campaign and a vault resource alike.
    fn gives_to(&self) -> Option<&OrgId>;

    /// Whether this caller may hand a record **to** `owner`: to
    /// [`gives_to`](Self::gives_to)'s org.
    fn may_hand_to(&self, owner: &PrincipalOwner) -> bool {
        self.is_server() || self.gives_to() == Some(owner.org_id())
    }
}

/// Refuse unless `scope` may put a record down on `owner`, then unless it
/// exists: scope first, so a refused caller learns nothing of existence.
pub async fn require_destination(
    state: &AppState,
    scope: &(impl HandOffScope + Sync),
    owner: &PrincipalOwner,
    refusal: &str,
) -> Result<(), AppError> {
    let org_id = owner.org_id();
    let exists = state
        .org_store
        .get_org(org_id)
        .await
        .map_err(|e| AppError::internal(e.context("failed to read the organization")))?
        .is_some();
    if !scope.may_hand_to(owner) {
        tracing::warn!(owner = %owner.label(), "refused: destination outside the caller's scope");
        return Err(AppError::Forbidden(refusal.into()));
    }
    if !exists {
        return Err(AppError::BadRequest(format!("no organization {org_id}")));
    }
    Ok(())
}

/// Whether the requested `org_id` admits `owner`. Applied inside a reach, so
/// an owner outside it gives an empty page rather than a `403`.
pub fn admits_filter(org_id: Option<&OrgId>, owner: &PrincipalOwner) -> bool {
    org_id.is_none_or(|named| named == owner.org_id())
}

/// A record an owned listing pages: whose it is, and the key the store lists
/// it by, which a cursor names.
pub trait Owned {
    type Key: Ord + std::fmt::Display + std::str::FromStr;
    fn owner(&self) -> &PrincipalOwner;
    fn key(&self) -> &Self::Key;
}

impl Owned for Campaign {
    type Key = CampaignId;
    fn owner(&self) -> &PrincipalOwner {
        &self.owner
    }
    fn key(&self) -> &CampaignId {
        &self.campaign_id
    }
}

impl Owned for CampaignGroup {
    type Key = CampaignGroupId;
    fn owner(&self) -> &PrincipalOwner {
        &self.owner
    }
    fn key(&self) -> &CampaignGroupId {
        &self.group_id
    }
}

impl Owned for VaultResource {
    type Key = VaultResourceId;
    fn owner(&self) -> &PrincipalOwner {
        &self.owner
    }
    fn key(&self) -> &VaultResourceId {
        &self.resource_id
    }
}

/// One page of owned records within the caller's reach, narrowed by the query's
/// `org_id`.
pub async fn page_within_reach<T, F, Fut>(
    config: &Config,
    query: OwnedListQuery,
    admits: impl Fn(&T) -> bool + Sync,
    what: &'static str,
    fetch: F,
) -> Result<OwnedPage<T>, AppError>
where
    T: Owned + Clone,
    F: FnMut(Option<T::Key>, NonZeroUsize) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<Page<T, T::Key>>>,
{
    let limit = list_limit(query.limit, config)?;
    let cursor = list_cursor(query.cursor, |cursor| cursor.parse::<T::Key>())?;
    let within = |item: &T| admits(item) && admits_filter(query.org_id.as_ref(), item.owner());
    Ok(match query.order.unwrap_or_default() {
        ListOrder::Oldest => OwnedPage {
            page: page_filtered(limit, cursor, what, within, fetch).await?,
            drained: None,
        },
        ListOrder::Newest => {
            let newest = page_newest(limit, cursor, what, within, T::key, fetch).await?;
            OwnedPage {
                page: newest.page,
                drained: Some(newest.drained),
            }
        }
    })
}

/// A page of owned records, and on a newest-first page the whole keyspace it
/// was drained from, every owner's.
pub struct OwnedPage<T> {
    pub page: ListPage<T>,
    pub drained: Option<Vec<T>>,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_core::types::Mint;

    use super::*;

    /// A scope built from the three facts alone, so the shared rule is asserted
    /// without either surface's vocabulary.
    struct Scope {
        server: bool,
        admin_of: Option<OrgId>,
        gives_to: Option<OrgId>,
    }

    impl HandOffScope for Scope {
        fn is_server(&self) -> bool {
            self.server
        }
        fn admin_of(&self) -> Option<&OrgId> {
            self.admin_of.as_ref()
        }
        fn gives_to(&self) -> Option<&OrgId> {
            self.gives_to.as_ref()
        }
    }

    fn org(id: &OrgId) -> PrincipalOwner {
        PrincipalOwner::Org { org_id: id.clone() }
    }

    /// The door the rule opens, and the ones it leaves shut.
    #[test]
    fn a_hand_off_may_land_on_the_callers_organization() {
        let my_org = OrgId::mint();
        let member = Scope {
            admin_of: None,
            server: false,
            gives_to: Some(my_org.clone()),
        };
        assert!(member.may_hand_to(&org(&my_org)));
        assert!(!member.may_hand_to(&org(&OrgId::mint())));

        let unaffiliated = Scope {
            server: false,
            admin_of: None,
            gives_to: None,
        };
        assert!(!unaffiliated.may_hand_to(&org(&my_org)));
    }

    #[test]
    fn the_server_may_hand_a_record_anywhere() {
        let scope = Scope {
            server: true,
            admin_of: None,
            gives_to: None,
        };
        assert!(scope.may_hand_to(&org(&OrgId::mint())));
    }

    /// The filter narrows inside a reach and never widens.
    #[rstest]
    #[case::none_admits_everything(false, true)]
    #[case::org_id_narrows(true, false)]
    fn the_owner_filter_only_narrows(#[case] other_org: bool, #[case] admitted: bool) {
        let my_org = OrgId::mint();
        let wanted = other_org.then(OrgId::mint);
        assert_eq!(admits_filter(wanted.as_ref(), &org(&my_org)), admitted);
    }

    /// Every filter's label parses back and is its serde form, over `EnumIter`
    /// so a new variant is covered: a link echoes the filter.
    #[test]
    fn an_owner_filter_label_is_a_value_the_route_accepts() -> anyhow::Result<()> {
        <pipette_plan_types::OwnerFilter as strum::IntoEnumIterator>::iter().try_for_each(
            |filter| {
                assert_eq!(
                    filter.label().parse::<pipette_plan_types::OwnerFilter>()?,
                    filter
                );
                assert_eq!(
                    serde_json::to_string(&filter)?,
                    format!("\"{}\"", filter.label())
                );
                Ok(())
            },
        )
    }
}
