//! [`S3OrgStore`]: the organization roster and its two relations on an object
//! store, under the `[storage]` bucket and prefix. The mechanics live in
//! [`super::org_layout`].

use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_plan_types::{ClientId, ClientOwner, OrgId, UserId};
use pipette_store::{OrgStore, SlugClaim};

use super::org_layout;

/// Addresses no principal keyspace: nothing here authenticates a request.
#[derive(Clone)]
pub struct S3OrgStore {
    store: Arc<dyn ObjectStore>,
    prefix: String,
}

impl S3OrgStore {
    pub fn new(store: Arc<dyn ObjectStore>, prefix: String) -> Self {
        Self { store, prefix }
    }
}

#[async_trait]
impl OrgStore for S3OrgStore {
    async fn get_org(&self, org_id: &OrgId) -> anyhow::Result<Option<Organization>> {
        org_layout::load_org(&self.store, &self.prefix, org_id).await
    }

    async fn put_org(&self, org: &Organization) -> anyhow::Result<()> {
        org_layout::save_org(&self.store, &self.prefix, org).await
    }

    async fn delete_org(&self, org_id: &OrgId) -> anyhow::Result<()> {
        org_layout::delete_org(&self.store, &self.prefix, org_id).await
    }

    async fn list_orgs(&self) -> anyhow::Result<Vec<Organization>> {
        org_layout::list_orgs(&self.store, &self.prefix).await
    }

    async fn claim_org_slug(&self, slug: &str, org_id: &OrgId) -> anyhow::Result<SlugClaim> {
        Ok(
            match org_layout::claim_slug(&self.store, &self.prefix, slug, org_id).await? {
                org_layout::Claim::Claimed => SlugClaim::Claimed,
                org_layout::Claim::Taken(holder) => SlugClaim::Taken(holder),
            },
        )
    }

    async fn lookup_org_by_slug(&self, slug: &str) -> anyhow::Result<Option<OrgId>> {
        org_layout::lookup_slug(&self.store, &self.prefix, slug).await
    }

    async fn release_org_slug(&self, slug: &str) -> anyhow::Result<()> {
        org_layout::release_slug(&self.store, &self.prefix, slug).await
    }

    async fn get_membership(&self, user_id: &UserId) -> anyhow::Result<Option<OrgId>> {
        org_layout::load_membership(&self.store, &self.prefix, user_id).await
    }

    async fn put_membership(&self, membership: &Membership) -> anyhow::Result<()> {
        org_layout::save_membership(&self.store, &self.prefix, membership).await
    }

    async fn delete_membership(&self, user_id: &UserId) -> anyhow::Result<()> {
        org_layout::delete_membership(&self.store, &self.prefix, user_id).await
    }

    async fn list_memberships(&self) -> anyhow::Result<Vec<Membership>> {
        org_layout::list_memberships(&self.store, &self.prefix).await
    }

    async fn get_ownership(&self, client_id: &ClientId) -> anyhow::Result<Option<ClientOwner>> {
        org_layout::load_ownership(&self.store, &self.prefix, client_id).await
    }

    async fn put_ownership(&self, ownership: &Ownership) -> anyhow::Result<()> {
        org_layout::save_ownership(&self.store, &self.prefix, ownership).await
    }

    async fn delete_ownership(&self, client_id: &ClientId) -> anyhow::Result<()> {
        org_layout::delete_ownership(&self.store, &self.prefix, client_id).await
    }

    async fn list_ownerships(&self) -> anyhow::Result<Vec<Ownership>> {
        org_layout::list_ownerships(&self.store, &self.prefix).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjPath;
    use object_store::{ObjectStore, ObjectStoreExt};

    use pipette_core::organization::{Membership, Organization, Ownership};
    use pipette_core::types::Mint;
    use pipette_plan_types::{ClientId, ClientOwner, NonEmptyTrimmedString, OrgSlug, UserId};
    use pipette_store::OrgStore;

    use super::S3OrgStore;

    fn org_store() -> (Arc<dyn ObjectStore>, S3OrgStore) {
        let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let org_store = S3OrgStore::new(store.clone(), "test-prefix".to_string());
        (store, org_store)
    }

    #[tokio::test]
    async fn test_s3_org_store_roundtrip() -> anyhow::Result<()> {
        let (_, store) = org_store();

        pipette_store_testkit::assert_org_store_roundtrip(&store).await
    }

    /// Each key is built from one constant, so a keyspace that moved is
    /// self-consistent and invisible to the round trip.
    #[tokio::test]
    async fn test_s3_org_keys() -> anyhow::Result<()> {
        let (backend, store) = org_store();

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
            backend
                .get(&ObjPath::from(format!("test-prefix/orgs/{org_id}.json")))
                .await
                .is_ok(),
            "the record must land under orgs/"
        );
        let claim = backend
            .get(&ObjPath::from("test-prefix/org-slug-index/acme"))
            .await?
            .bytes()
            .await?;
        assert_eq!(
            String::from_utf8(claim.to_vec())?,
            org_id.as_str(),
            "the claim must be keyed by the folded handle and hold the bare id"
        );

        // The relations are what a reader of `[storage]` joins on, so their
        // keys and bodies are a contract too.
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
                client_id,
                owner: ClientOwner::User {
                    user_id: user_id.clone(),
                },
            })
            .await?;
        let read = |key: String| {
            let backend = backend.clone();
            async move {
                let body = backend.get(&ObjPath::from(key)).await?.bytes().await?;
                anyhow::Ok(serde_json::from_slice::<serde_json::Value>(&body)?)
            }
        };
        assert_eq!(
            read(format!("test-prefix/user-orgs/{user_id}.json")).await?,
            serde_json::json!({ "user_id": user_id.as_str(), "org_id": org_id.as_str() })
        );
        assert_eq!(
            read("test-prefix/client-owners/ev1_rack.json".to_string()).await?,
            serde_json::json!({
                "client_id": "ev1_rack",
                "owner": { "type": "user", "user_id": user_id.as_str() },
            })
        );
        Ok(())
    }
}
