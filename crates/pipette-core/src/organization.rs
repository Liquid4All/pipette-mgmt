//! Organizations, and the two relations hung off them: which organization a
//! user belongs to, and who owns a device. Writing them is an authority: they
//! scope members' grants and what their devices reach.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use pipette_plan_types::{
    ClientId, ClientOwner, NonEmptyTrimmedString, OrgId, OrgSlug, OrgStatus, UserId,
};

use crate::types::Mint;

/// An organization — the body of `orgs/{org_id}.json`.
///
/// The slug is the index key, so it is stable and one path segment; the display
/// name is what somebody wrote. A rename touches the second only, so no stored
/// reference goes stale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Organization {
    pub org_id: OrgId,
    /// The handle, and the key of its `org-slug-index/{slug}` entry: unique
    /// across organizations by an exclusive create on that key.
    pub slug: OrgSlug,
    /// What to print for a human: `"Acme Corp., Inc."` where the slug is
    /// `acme`. Free-text and mutable; nothing keys on it.
    pub display_name: NonEmptyTrimmedString,
    pub status: OrgStatus,
    pub created_at: DateTime<Utc>,
}

impl From<Organization> for pipette_plan_types::Organization {
    fn from(org: Organization) -> Self {
        Self {
            org_id: org.org_id,
            slug: org.slug,
            display_name: org.display_name,
            status: org.status,
            created_at: org.created_at,
        }
    }
}

impl Organization {
    /// A new active organization under a freshly minted id. Infallible:
    /// [`OrgSlug`] has already folded the handle and refused an id-prefixed one.
    pub fn new(
        slug: OrgSlug,
        display_name: NonEmptyTrimmedString,
        created_at: DateTime<Utc>,
    ) -> Self {
        Self {
            org_id: OrgId::mint(),
            slug,
            display_name,
            status: OrgStatus::Active,
            created_at,
        }
    }
}

/// `user-orgs/{user_id}.json` in `[storage]`. An account in no organization has
/// no record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Membership {
    pub user_id: UserId,
    pub org_id: OrgId,
}

/// `client-owners/{client_id}.json` in `[storage]`. An unowned device has no
/// record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ownership {
    pub client_id: ClientId,
    pub owner: ClientOwner,
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    fn an_org() -> anyhow::Result<Organization> {
        Ok(Organization::new(
            OrgSlug::try_new("acme")?,
            NonEmptyTrimmedString::try_new("Acme Corp., Inc.")?,
            Utc::now(),
        ))
    }

    /// A fresh organization is in service and carries a minted id — nothing a
    /// caller chose.
    #[test]
    fn a_new_organization_is_active_with_a_minted_id() -> anyhow::Result<()> {
        let org = an_org()?;
        assert_eq!(org.status, OrgStatus::Active);
        assert!(org.org_id.as_str().starts_with(OrgId::PREFIX));
        assert_ne!(org.org_id, an_org()?.org_id, "each create mints its own id");
        Ok(())
    }

    /// A handle may not be spelled like an id. The two namespaces are told
    /// apart by that prefix alone, so a handle carrying it would create an
    /// organization reachable only by its minted id.
    #[test]
    fn a_handle_cannot_be_spelled_like_an_id() {
        assert!(OrgSlug::try_new("org_acme").is_err());
        assert!(
            OrgSlug::try_new(" ORG_ACME ").is_err(),
            "folded before checked"
        );
        assert!(
            OrgSlug::try_new("organisation").is_ok(),
            "a bare prefix match only"
        );
    }

    /// The slug folds and the display name does not. Two operators typing the
    /// organization's name differently must reach one index key, while the
    /// printable name keeps whatever capitalization somebody chose.
    #[rstest]
    #[case(" ACME ", "acme")]
    #[case("Liquid-Edge", "liquid-edge")]
    fn the_slug_folds_and_the_display_name_does_not(
        #[case] typed: &str,
        #[case] folded: &str,
    ) -> anyhow::Result<()> {
        let org = Organization::new(
            OrgSlug::try_new(typed)?,
            NonEmptyTrimmedString::try_new("Acme Corp., Inc.")?,
            Utc::now(),
        );
        assert_eq!(org.slug.as_str(), folded);
        assert_eq!(org.display_name.as_str(), "Acme Corp., Inc.");
        Ok(())
    }
}
