//! [`LocalFsOrgStore`]: the organization roster and its two relations on a
//! filesystem, under the `[storage]` root. The mechanics live in
//! [`super::org_layout`].

use std::path::PathBuf;

use async_trait::async_trait;

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserId};
use pipette_store::{OrgStore, SlugClaim};

use super::{org_layout, run_blocking};

/// Holds no principal directory: nothing here authenticates a request.
#[derive(Clone)]
pub struct LocalFsOrgStore {
    orgs_dir: PathBuf,
    org_slug_index_dir: PathBuf,
    user_orgs_dir: PathBuf,
    client_owners_dir: PathBuf,
}

impl LocalFsOrgStore {
    /// `root` is the `[storage]` data directory.
    pub fn new(root: PathBuf) -> Self {
        Self {
            orgs_dir: root.join("orgs"),
            org_slug_index_dir: root.join("org-slug-index"),
            user_orgs_dir: root.join("user-orgs"),
            client_owners_dir: root.join("client-owners"),
        }
    }
}

#[async_trait]
impl OrgStore for LocalFsOrgStore {
    async fn get_org(&self, org_id: &OrgId) -> anyhow::Result<Option<Organization>> {
        let dir = self.orgs_dir.clone();
        let org_id = org_id.clone();
        run_blocking(move || org_layout::load_org(&dir, &org_id)).await
    }

    async fn put_org(&self, org: &Organization) -> anyhow::Result<()> {
        let dir = self.orgs_dir.clone();
        let org = org.clone();
        run_blocking(move || org_layout::save_org(&dir, &org)).await
    }

    async fn delete_org(&self, org_id: &OrgId) -> anyhow::Result<()> {
        let dir = self.orgs_dir.clone();
        let org_id = org_id.clone();
        run_blocking(move || org_layout::delete_org(&dir, &org_id)).await
    }

    async fn list_orgs(&self) -> anyhow::Result<Vec<Organization>> {
        let dir = self.orgs_dir.clone();
        run_blocking(move || org_layout::list_orgs(&dir)).await
    }

    async fn claim_org_slug(&self, slug: &str, org_id: &OrgId) -> anyhow::Result<SlugClaim> {
        let dir = self.org_slug_index_dir.clone();
        let slug = slug.to_string();
        let org_id = org_id.clone();
        let claim = run_blocking(move || org_layout::claim_slug(&dir, &slug, &org_id)).await?;
        Ok(match claim {
            org_layout::Claim::Claimed => SlugClaim::Claimed,
            org_layout::Claim::Taken(holder) => SlugClaim::Taken(holder),
        })
    }

    async fn lookup_org_by_slug(&self, slug: &str) -> anyhow::Result<Option<OrgId>> {
        let dir = self.org_slug_index_dir.clone();
        let slug = slug.to_string();
        run_blocking(move || org_layout::lookup_slug(&dir, &slug)).await
    }

    async fn release_org_slug(&self, slug: &str) -> anyhow::Result<()> {
        let dir = self.org_slug_index_dir.clone();
        let slug = slug.to_string();
        run_blocking(move || org_layout::release_slug(&dir, &slug)).await
    }

    async fn get_membership(&self, user_id: &UserId) -> anyhow::Result<Option<OrgId>> {
        let dir = self.user_orgs_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || org_layout::load_membership(&dir, &user_id)).await
    }

    async fn put_membership(&self, membership: &Membership) -> anyhow::Result<()> {
        let dir = self.user_orgs_dir.clone();
        let membership = membership.clone();
        run_blocking(move || org_layout::save_membership(&dir, &membership)).await
    }

    async fn delete_membership(&self, user_id: &UserId) -> anyhow::Result<()> {
        let dir = self.user_orgs_dir.clone();
        let user_id = user_id.clone();
        run_blocking(move || org_layout::delete_membership(&dir, &user_id)).await
    }

    async fn list_memberships(&self) -> anyhow::Result<Vec<Membership>> {
        let dir = self.user_orgs_dir.clone();
        run_blocking(move || org_layout::list_memberships(&dir)).await
    }

    async fn get_ownership(&self, client_id: &ClientId) -> anyhow::Result<Option<ClientOwner>> {
        let dir = self.client_owners_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || org_layout::load_ownership(&dir, &client_id)).await
    }

    async fn put_ownership(&self, ownership: &Ownership) -> anyhow::Result<()> {
        let dir = self.client_owners_dir.clone();
        let ownership = ownership.clone();
        run_blocking(move || org_layout::save_ownership(&dir, &ownership)).await
    }

    async fn delete_ownership(&self, client_id: &ClientId) -> anyhow::Result<()> {
        let dir = self.client_owners_dir.clone();
        let client_id = client_id.clone();
        run_blocking(move || org_layout::delete_ownership(&dir, &client_id)).await
    }

    async fn list_ownerships(&self) -> anyhow::Result<Vec<Ownership>> {
        let dir = self.client_owners_dir.clone();
        run_blocking(move || org_layout::list_ownerships(&dir)).await
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use pipette_core::organization::{Membership, Organization, Ownership};
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        ClientId, ClientOwner, NonEmptyTrimmedString, OrgId, OrgSlug, UserId,
    };
    use pipette_store::{OrgStore, SlugClaim};

    use super::LocalFsOrgStore;

    #[tokio::test]
    async fn test_localfs_org_store_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsOrgStore::new(dir.path().to_path_buf());

        pipette_store_testkit::assert_org_store_roundtrip(&store).await
    }

    /// Each path is built from one field, so a directory that moved is
    /// self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_localfs_org_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsOrgStore::new(dir.path().to_path_buf());

        // Typed with capitals and padding; `OrgSlug` folds, so one handle is
        // one key whichever way an operator typed it.
        let org = Organization::new(
            OrgSlug::try_new(" ACME ")?,
            NonEmptyTrimmedString::try_new("Acme Corp., Inc.")?,
            Utc::now(),
        );
        let org_id = org.org_id.clone();
        store.claim_org_slug(org.slug.as_str(), &org_id).await?;
        store.put_org(&org).await?;

        assert!(
            dir.path().join(format!("orgs/{org_id}.json")).exists(),
            "the record must land under orgs/"
        );
        let claim = dir.path().join("org-slug-index/acme");
        assert!(
            claim.exists(),
            "the claim must be keyed by the folded handle"
        );
        assert_eq!(
            std::fs::read_to_string(&claim)?,
            org_id.as_str(),
            "the claim body is the bare id"
        );

        // The relations are what a reader of `[storage]` joins on, so their
        // paths and bodies are a contract too.
        let user_id = UserId::mint();
        store
            .put_membership(&Membership {
                user_id: user_id.clone(),
                org_id: org_id.clone(),
            })
            .await?;
        let client_id = ClientId::try_new("ev1_rack")?;
        store
            .put_ownership(&Ownership {
                client_id: client_id.clone(),
                owner: ClientOwner::Org {
                    org_id: org_id.clone(),
                },
            })
            .await?;
        let read = |path: String| -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::from_slice(&std::fs::read(
                dir.path().join(path),
            )?)?)
        };
        assert_eq!(
            read(format!("user-orgs/{user_id}.json"))?,
            serde_json::json!({ "user_id": user_id.as_str(), "org_id": org_id.as_str() })
        );
        assert_eq!(
            read(format!("client-owners/{client_id}.json"))?,
            serde_json::json!({
                "client_id": "ev1_rack",
                "owner": { "type": "org", "org_id": org_id.as_str() },
            })
        );
        Ok(())
    }

    /// Concurrent creates of one handle make one winner, and every loser reads
    /// the winner's id back rather than the empty file of a claim still being
    /// written.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn test_localfs_racing_claims_name_the_winner() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = LocalFsOrgStore::new(dir.path().to_path_buf());

        for round in 0..50 {
            let slug = format!("race-{round}");
            let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
            let racers: Vec<_> = (0..16)
                .map(|_| {
                    let (store, slug, barrier) = (store.clone(), slug.clone(), barrier.clone());
                    tokio::spawn(async move {
                        let org_id = OrgId::mint();
                        barrier.wait().await;
                        let claim = store.claim_org_slug(&slug, &org_id).await?;
                        anyhow::Ok((org_id, claim))
                    })
                })
                .collect();
            let claims = futures::future::try_join_all(racers)
                .await?
                .into_iter()
                .collect::<anyhow::Result<Vec<_>>>()?;
            let winners: Vec<&OrgId> = claims
                .iter()
                .filter_map(|(org_id, claim)| matches!(claim, SlugClaim::Claimed).then_some(org_id))
                .collect();
            assert_eq!(winners.len(), 1, "round {round}: exactly one claim wins");
            assert!(
                claims.iter().all(|(_, claim)| match claim {
                    SlugClaim::Claimed => true,
                    SlugClaim::Taken(holder) => winners.first() == Some(&holder),
                }),
                "round {round}: every loser names the winner"
            );
        }
        assert_eq!(
            std::fs::read_dir(dir.path().join("org-slug-index"))?.count(),
            50,
            "no staging file outlives its claim"
        );
        Ok(())
    }
}
