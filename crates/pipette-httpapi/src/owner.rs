//! Where an owned record may be **put down**, and which slice of a listing a
//! caller asked for, shared by the campaign and vault routes so the two cannot
//! drift.

use std::num::NonZeroUsize;

use pipette_core::campaign::Campaign;
use pipette_core::config::Config;
use pipette_core::vault::VaultResource;
use pipette_plan_types::{
    CampaignId, ListOrder, ListPage, OrgId, OwnedListQuery, OwnerFilter, PrincipalOwner, UserId,
    VaultResourceId,
};
use pipette_store::Page;

use crate::auth::user_org;
use crate::error::AppError;
use crate::list::{list_cursor, list_limit, page_filtered, page_newest};
use crate::state::AppState;

/// How far a caller may reach when it hands a record to somebody: four facts,
/// not one scope, since a caller can hold several at once.
pub trait HandOffScope {
    fn user_id(&self) -> &UserId;
    /// Whether this caller reaches every record, whoever owns it.
    fn is_server(&self) -> bool;
    /// The organization this caller administers: the grant **and** membership.
    fn admin_of(&self) -> Option<&OrgId>;
    /// The organization this caller may put a record **of its own** down on:
    /// its membership, for a campaign and a vault resource alike.
    fn gives_to(&self) -> Option<&OrgId>;

    /// Whether this caller may hand a record **to** `owner`: an org admin to
    /// its members, anyone its own to [`gives_to`](Self::gives_to)'s org.
    /// `destination_org` is resolved by the caller: this trait holds no store.
    fn may_hand_to(&self, owner: &PrincipalOwner, destination_org: Option<&OrgId>) -> bool {
        if self.is_server() {
            return true;
        }
        match owner {
            PrincipalOwner::User { user_id } => {
                user_id == self.user_id()
                    // `is_some` first: two absent organizations are not a match,
                    // or a caller administering nothing could hand a record to
                    // any unaffiliated account.
                    || (self.admin_of().is_some() && self.admin_of() == destination_org)
            }
            PrincipalOwner::Org { org_id } => self.gives_to() == Some(org_id),
        }
    }
}

/// Where a record would land, read once for the scope and existence checks.
enum Destination {
    Absent,
    On(Option<OrgId>),
}

async fn destination(state: &AppState, owner: &PrincipalOwner) -> Result<Destination, AppError> {
    match owner {
        PrincipalOwner::Org { org_id } => Ok(state
            .org_store
            .get_org(org_id)
            .await
            .map_err(|e| AppError::internal(e.context("failed to read the organization")))?
            .map_or(Destination::Absent, |org| Destination::On(Some(org.org_id)))),
        PrincipalOwner::User { user_id } => {
            let exists = state
                .user_store
                .get_user(user_id)
                .await
                .map_err(|e| AppError::internal(e.context("failed to read the account")))?
                .is_some();
            if !exists {
                return Ok(Destination::Absent);
            }
            Ok(Destination::On(user_org(state, user_id).await?))
        }
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
    let destination = destination(state, owner).await?;
    let on = match &destination {
        Destination::On(org) => org.as_ref(),
        Destination::Absent => None,
    };
    if !scope.may_hand_to(owner, on) {
        tracing::warn!(owner = %owner.label(), "refused: destination outside the caller's scope");
        return Err(AppError::Forbidden(refusal.into()));
    }
    match destination {
        Destination::On(_) => Ok(()),
        Destination::Absent => Err(AppError::BadRequest(match owner {
            PrincipalOwner::User { user_id } => format!("no account {user_id}"),
            PrincipalOwner::Org { org_id } => format!("no organization {org_id}"),
        })),
    }
}

/// Whether the requested filter admits `owner`. Applied inside a reach, so an
/// owner outside it gives an empty page rather than a `403`.
pub fn admits_filter(
    owner_filter: Option<OwnerFilter>,
    org_id: Option<&OrgId>,
    owner: &PrincipalOwner,
    caller: &UserId,
) -> bool {
    match owner_filter {
        Some(OwnerFilter::Mine) => owner.user_id() == Some(caller),
        // `owner=org` with no `org_id` is every organization-owned record in
        // reach; with one, that organization's alone.
        Some(OwnerFilter::Org) => owner
            .org_id()
            .is_some_and(|held| org_id.is_none_or(|named| named == held)),
        // A bare `org_id` narrows without having to also say `owner=org`.
        None => org_id.is_none_or(|named| owner.org_id() == Some(named)),
    }
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
/// owner filter.
pub async fn page_within_reach<T, F, Fut>(
    config: &Config,
    query: OwnedListQuery,
    caller: &UserId,
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
    let within = |item: &T| {
        admits(item) && admits_filter(query.owner, query.org_id.as_ref(), item.owner(), caller)
    };
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

    /// A scope built from the four facts alone, so the shared rule is asserted
    /// without either surface's vocabulary.
    struct Scope {
        user_id: UserId,
        server: bool,
        admin_of: Option<OrgId>,
        gives_to: Option<OrgId>,
    }

    impl HandOffScope for Scope {
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
            self.gives_to.as_ref()
        }
    }

    fn org(id: &OrgId) -> PrincipalOwner {
        PrincipalOwner::Org { org_id: id.clone() }
    }

    fn user(id: &UserId) -> PrincipalOwner {
        PrincipalOwner::User {
            user_id: id.clone(),
        }
    }

    /// The two doors the rule opens, and the ones it leaves shut.
    #[test]
    fn a_hand_off_may_land_anywhere_inside_the_roster() {
        let me = UserId::mint();
        let my_org = OrgId::mint();
        let colleague = UserId::mint();
        let admin = Scope {
            user_id: me.clone(),
            server: false,
            admin_of: Some(my_org.clone()),
            gives_to: Some(my_org.clone()),
        };

        assert!(admin.may_hand_to(&user(&colleague), Some(&my_org)));
        let member = Scope {
            admin_of: None,
            user_id: me.clone(),
            server: false,
            gives_to: Some(my_org.clone()),
        };
        assert!(member.may_hand_to(&org(&my_org), Some(&my_org)));
        assert!(member.may_hand_to(&user(&me), Some(&my_org)));

        assert!(!admin.may_hand_to(&user(&colleague), Some(&OrgId::mint())));
        assert!(!admin.may_hand_to(&user(&colleague), None));

        let unaffiliated = Scope {
            user_id: me,
            server: false,
            admin_of: None,
            gives_to: None,
        };
        assert!(!unaffiliated.may_hand_to(&user(&colleague), None));
        assert!(!unaffiliated.may_hand_to(&org(&my_org), Some(&my_org)));
    }

    #[test]
    fn the_server_may_hand_a_record_anywhere() {
        let scope = Scope {
            user_id: UserId::mint(),
            server: true,
            admin_of: None,
            gives_to: None,
        };
        assert!(scope.may_hand_to(&user(&UserId::mint()), Some(&OrgId::mint())));
        assert!(scope.may_hand_to(&org(&OrgId::mint()), None));
    }

    /// The filter narrows inside a reach and never widens: `mine` never admits
    /// an organization's record, and the two words partition the listing.
    #[rstest]
    #[case::none_admits_everything(None, false, true)]
    #[case::mine_excludes_org(Some(OwnerFilter::Mine), false, false)]
    #[case::org_includes_org(Some(OwnerFilter::Org), false, true)]
    #[case::org_id_narrows(Some(OwnerFilter::Org), true, false)]
    fn the_owner_filter_only_narrows(
        #[case] filter: Option<OwnerFilter>,
        #[case] other_org: bool,
        #[case] admitted: bool,
    ) {
        let caller = UserId::mint();
        let my_org = OrgId::mint();
        let wanted = other_org.then(OrgId::mint);
        assert_eq!(
            admits_filter(filter, wanted.as_ref(), &org(&my_org), &caller),
            admitted
        );
    }

    /// Every filter's label parses back and is its serde form, over `EnumIter`
    /// so a new variant is covered: a link echoes the filter.
    #[test]
    fn an_owner_filter_label_is_a_value_the_route_accepts() -> anyhow::Result<()> {
        <OwnerFilter as strum::IntoEnumIterator>::iter().try_for_each(|filter| {
            assert_eq!(filter.label().parse::<OwnerFilter>()?, filter);
            assert_eq!(
                serde_json::to_string(&filter)?,
                format!("\"{}\"", filter.label())
            );
            Ok(())
        })
    }

    #[test]
    fn mine_and_org_partition_the_listing() {
        let caller = UserId::mint();
        let mine = user(&caller);
        assert!(admits_filter(Some(OwnerFilter::Mine), None, &mine, &caller));
        assert!(!admits_filter(Some(OwnerFilter::Org), None, &mine, &caller));
        assert!(admits_filter(None, None, &mine, &caller));
    }
}
