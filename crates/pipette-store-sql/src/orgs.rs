//! [`OrgStore`] on SQL, one table per keyspace with no foreign keys: a claim
//! precedes its record, and a relation may outlive its organization.

use anyhow::Context;
use async_trait::async_trait;

use pipette_core::organization::{Membership, Organization, Ownership};
use pipette_plan_types::{ClientId, ClientOwner, OrgId, PrincipalIdError, UserId};
use pipette_store::{OrgStore, SlugClaim};

use crate::ClaimVanishedError;
use crate::db::SqlDatabase;
use crate::listing::Keyspace;
use crate::retry::retry;

const ORGS: Keyspace = Keyspace {
    table: "orgs",
    id_column: "org_id",
    report_prefix: "orgs/",
};

fn owner_columns(owner: &ClientOwner) -> (&'static str, &str) {
    match owner {
        ClientOwner::Org { org_id } => ("org", org_id.as_str()),
        ClientOwner::User { user_id } => ("user", user_id.as_str()),
    }
}

/// An ownership row that does not name an owner.
#[derive(Debug, thiserror::Error)]
enum OwnerError {
    #[error(transparent)]
    Id(#[from] PrincipalIdError),
    #[error("unknown owner type {0:?}")]
    UnknownType(String),
}

fn owner_of(client_id: &str, owner_type: &str, owner_id: &str) -> anyhow::Result<ClientOwner> {
    let owner: Result<ClientOwner, OwnerError> = match owner_type {
        "org" => OrgId::try_new(owner_id)
            .map(|org_id| ClientOwner::Org { org_id })
            .map_err(Into::into),
        "user" => UserId::try_new(owner_id)
            .map(|user_id| ClientOwner::User { user_id })
            .map_err(Into::into),
        other => Err(OwnerError::UnknownType(other.to_owned())),
    };
    owner.with_context(|| format!("client-owners/{client_id}.json"))
}

fn member_of(user_id: &str, org_id: &str) -> anyhow::Result<OrgId> {
    OrgId::try_new(org_id).with_context(|| format!("user-orgs/{user_id}.json"))
}

/// Accept padding in a claim body, as the object stores do.
fn holder(slug: &str, body: &str) -> anyhow::Result<OrgId> {
    OrgId::try_new(body.trim())
        .with_context(|| format!("handle claim for {slug} names no organization"))
}

pub(crate) struct SqlOrgStore {
    db: SqlDatabase,
}

impl SqlOrgStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

#[async_trait]
impl OrgStore for SqlOrgStore {
    async fn get_org(&self, org_id: &OrgId) -> anyhow::Result<Option<Organization>> {
        let id = org_id.as_str();
        let body: Option<(String,)> = self
            .db
            .fetch_optional("get_org", || {
                sqlx::query_as("SELECT body FROM orgs WHERE org_id = $1").bind(id)
            })
            .await?;
        body.map(|(body,)| serde_json::from_str(&body).with_context(|| ORGS.report_key(id)))
            .transpose()
    }

    async fn put_org(&self, org: &Organization) -> anyhow::Result<()> {
        let body = serde_json::to_string_pretty(org)?;
        self.db
            .execute("put_org", || {
                sqlx::query(
                    "INSERT INTO orgs (org_id, body) VALUES ($1, $2)
                     ON CONFLICT (org_id) DO UPDATE SET body = EXCLUDED.body",
                )
                .bind(org.org_id.as_str())
                .bind(body.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_org(&self, org_id: &OrgId) -> anyhow::Result<()> {
        self.db
            .execute("delete_org", || {
                sqlx::query("DELETE FROM orgs WHERE org_id = $1").bind(org_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// In the object stores' key order (`{org_id}.json`), logging and skipping
    /// a record that does not parse, as they do.
    async fn list_orgs(&self) -> anyhow::Result<Vec<Organization>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_orgs", || {
                sqlx::query_as("SELECT org_id, body FROM orgs ORDER BY (org_id || '.json')")
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(id, body)| match serde_json::from_str(&body) {
                Ok(record) => Some(record),
                Err(e) => {
                    tracing::warn!(key = %ORGS.report_key(&id), error = %e, "skipping malformed record");
                    None
                }
            })
            .collect())
    }

    async fn claim_org_slug(&self, slug: &str, org_id: &OrgId) -> anyhow::Result<SlugClaim> {
        let db = &self.db;
        let claimant = org_id.as_str();
        // A racing claimant fails at the insert; the retry then reads the winner.
        let (claimed, incumbent) = retry("claim_org_slug", || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let claimed = sqlx::query(
                    "INSERT INTO org_slug_index (slug, org_id) VALUES ($1, $2)
                     ON CONFLICT (slug) DO NOTHING",
                )
                .bind(slug)
                .bind(claimant)
                .execute(&mut *tx)
                .await?
                .rows_affected()
                    == 1;
                let incumbent: Option<String> = if claimed {
                    None
                } else {
                    sqlx::query_scalar("SELECT org_id FROM org_slug_index WHERE slug = $1")
                        .bind(slug)
                        .fetch_optional(&mut *tx)
                        .await?
                };
                tx.commit().await?;
                Ok((claimed, incumbent))
            })
        })
        .await?;
        if claimed {
            return Ok(SlugClaim::Claimed);
        }
        let incumbent = incumbent.ok_or_else(|| ClaimVanishedError {
            claim: "handle",
            name: slug.to_owned(),
        })?;
        Ok(SlugClaim::Taken(holder(slug, &incumbent)?))
    }

    async fn lookup_org_by_slug(&self, slug: &str) -> anyhow::Result<Option<OrgId>> {
        let incumbent: Option<(String,)> = self
            .db
            .fetch_optional("lookup_org_by_slug", || {
                sqlx::query_as("SELECT org_id FROM org_slug_index WHERE slug = $1").bind(slug)
            })
            .await?;
        incumbent.map(|(body,)| holder(slug, &body)).transpose()
    }

    async fn release_org_slug(&self, slug: &str) -> anyhow::Result<()> {
        self.db
            .execute("release_org_slug", || {
                sqlx::query("DELETE FROM org_slug_index WHERE slug = $1").bind(slug)
            })
            .await?;
        Ok(())
    }

    async fn get_membership(&self, user_id: &UserId) -> anyhow::Result<Option<OrgId>> {
        let org_id: Option<(String,)> = self
            .db
            .fetch_optional("get_membership", || {
                sqlx::query_as("SELECT org_id FROM user_orgs WHERE user_id = $1")
                    .bind(user_id.as_str())
            })
            .await?;
        org_id
            .map(|(org_id,)| member_of(user_id.as_str(), &org_id))
            .transpose()
    }

    async fn put_membership(&self, membership: &Membership) -> anyhow::Result<()> {
        self.db
            .execute("put_membership", || {
                sqlx::query(
                    "INSERT INTO user_orgs (user_id, org_id) VALUES ($1, $2)
                     ON CONFLICT (user_id) DO UPDATE SET org_id = EXCLUDED.org_id",
                )
                .bind(membership.user_id.as_str())
                .bind(membership.org_id.as_str())
            })
            .await?;
        Ok(())
    }

    async fn delete_membership(&self, user_id: &UserId) -> anyhow::Result<()> {
        self.db
            .execute("delete_membership", || {
                sqlx::query("DELETE FROM user_orgs WHERE user_id = $1").bind(user_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// In the object stores' key order (`{user_id}.json`), logging and skipping
    /// a row that names no valid ids.
    async fn list_memberships(&self) -> anyhow::Result<Vec<Membership>> {
        let rows: Vec<(String, String)> = self
            .db
            .fetch_all("list_memberships", || {
                sqlx::query_as(
                    "SELECT user_id, org_id FROM user_orgs ORDER BY (user_id || '.json')",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(user_id, org_id)| {
                let membership = UserId::try_new(user_id.as_str())
                    .map_err(anyhow::Error::from)
                    .and_then(|parsed| {
                        Ok(Membership {
                            org_id: member_of(&user_id, &org_id)?,
                            user_id: parsed,
                        })
                    });
                membership
                    .inspect_err(|e| {
                        tracing::warn!(user_id = %user_id, error = %format!("{e:#}"),
                            "skipping malformed membership row");
                    })
                    .ok()
            })
            .collect())
    }

    async fn get_ownership(&self, client_id: &ClientId) -> anyhow::Result<Option<ClientOwner>> {
        let row: Option<(String, String)> = self
            .db
            .fetch_optional("get_ownership", || {
                sqlx::query_as(
                    "SELECT owner_type, owner_id FROM client_owners WHERE client_id = $1",
                )
                .bind(client_id.as_str())
            })
            .await?;
        row.map(|(owner_type, owner_id)| owner_of(client_id.as_str(), &owner_type, &owner_id))
            .transpose()
    }

    async fn put_ownership(&self, ownership: &Ownership) -> anyhow::Result<()> {
        let (owner_type, owner_id) = owner_columns(&ownership.owner);
        self.db
            .execute("put_ownership", || {
                sqlx::query(
                    "INSERT INTO client_owners (client_id, owner_type, owner_id) VALUES ($1, $2, $3)
                     ON CONFLICT (client_id) DO UPDATE SET
                         owner_type = EXCLUDED.owner_type,
                         owner_id = EXCLUDED.owner_id",
                )
                .bind(ownership.client_id.as_str())
                .bind(owner_type)
                .bind(owner_id)
            })
            .await?;
        Ok(())
    }

    async fn delete_ownership(&self, client_id: &ClientId) -> anyhow::Result<()> {
        self.db
            .execute("delete_ownership", || {
                sqlx::query("DELETE FROM client_owners WHERE client_id = $1")
                    .bind(client_id.as_str())
            })
            .await?;
        Ok(())
    }

    /// In the object stores' key order (`{client_id}.json`), logging and
    /// skipping a row that names no valid ids.
    async fn list_ownerships(&self) -> anyhow::Result<Vec<Ownership>> {
        let rows: Vec<(String, String, String)> = self
            .db
            .fetch_all("list_ownerships", || {
                sqlx::query_as(
                    "SELECT client_id, owner_type, owner_id FROM client_owners
                     ORDER BY (client_id || '.json')",
                )
            })
            .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(client_id, owner_type, owner_id)| {
                let ownership = ClientId::try_new(client_id.as_str())
                    .map_err(anyhow::Error::from)
                    .and_then(|parsed| {
                        Ok(Ownership {
                            owner: owner_of(&client_id, &owner_type, &owner_id)?,
                            client_id: parsed,
                        })
                    });
                ownership
                    .inspect_err(|e| {
                        tracing::warn!(client_id = %client_id, error = %format!("{e:#}"),
                            "skipping malformed ownership row");
                    })
                    .ok()
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;

    use pipette_core::organization::{Membership, Organization, Ownership};
    use pipette_core::types::Mint;
    use pipette_plan_types::{
        ClientId, ClientOwner, NonEmptyTrimmedString, OrgId, OrgSlug, UserId,
    };
    use pipette_store::{OrgStore, SlugClaim};

    use super::SqlOrgStore;
    use crate::db::SqlDatabase;
    use crate::testing::{RACERS, race};

    async fn body(db: &SqlDatabase, sql: &'static str, id: &str) -> anyhow::Result<String> {
        Ok(sqlx::query_scalar(sql)
            .bind(id)
            .fetch_one(db.pool())
            .await?)
    }

    async fn insert_raw(
        db: &SqlDatabase,
        sql: &'static str,
        id: &str,
        body: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(sql)
            .bind(id)
            .bind(body)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn sql_org_store_roundtrip() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlOrgStore::new(test_db.db().clone());
        pipette_store_testkit::assert_org_store_roundtrip(&store).await
    }

    #[tokio::test]
    async fn sql_org_rows_hold_the_records() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlOrgStore::new(db.clone());

        let org = Organization::new(
            OrgSlug::try_new(" ACME ")?,
            NonEmptyTrimmedString::try_new("Acme Corp., Inc.")?,
            Utc::now(),
        );
        let org_id = org.org_id.clone();
        store.claim_org_slug(org.slug.as_str(), &org_id).await?;
        store.put_org(&org).await?;
        assert_eq!(
            body(
                db,
                "SELECT body FROM orgs WHERE org_id = $1",
                org_id.as_str()
            )
            .await?,
            serde_json::to_string_pretty(&org)?
        );
        assert_eq!(
            body(
                db,
                "SELECT org_id FROM org_slug_index WHERE slug = $1",
                "acme"
            )
            .await?,
            org_id.as_str(),
            "the claim is keyed by the folded handle and holds the bare id"
        );

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
        assert_eq!(
            body(
                db,
                "SELECT org_id FROM user_orgs WHERE user_id = $1",
                user_id.as_str(),
            )
            .await?,
            org_id.as_str()
        );
        let owner: (String, String) =
            sqlx::query_as("SELECT owner_type, owner_id FROM client_owners WHERE client_id = $1")
                .bind(client_id.as_str())
                .fetch_one(db.pool())
                .await?;
        assert_eq!(owner, ("org".to_string(), org_id.to_string()));
        Ok(())
    }

    /// A row that no store write makes fails its read and leaves its listing.
    #[tokio::test]
    async fn sql_org_store_skips_a_malformed_record() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let store = SqlOrgStore::new(db.clone());
        let broken_org = OrgId::mint();
        insert_raw(
            db,
            "INSERT INTO orgs (org_id, body) VALUES ($1, $2)",
            broken_org.as_str(),
            "{not json",
        )
        .await?;
        let broken_user = UserId::mint();
        insert_raw(
            db,
            "INSERT INTO user_orgs (user_id, org_id) VALUES ($1, $2)",
            broken_user.as_str(),
            "not-an-org",
        )
        .await?;
        let broken_client = ClientId::try_new("ev1_broken")?;
        insert_raw(
            db,
            "INSERT INTO client_owners (client_id, owner_type, owner_id) VALUES ($1, 'org', $2)",
            broken_client.as_str(),
            "not-an-org",
        )
        .await?;

        assert!(store.get_org(&broken_org).await.is_err());
        assert!(store.list_orgs().await?.is_empty());
        assert!(store.get_membership(&broken_user).await.is_err());
        assert!(store.list_memberships().await?.is_empty());
        assert!(store.get_ownership(&broken_client).await.is_err());
        assert!(store.list_ownerships().await?.is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn sql_ownerships_list_in_key_order() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = SqlOrgStore::new(test_db.db().clone());
        let owner = ClientOwner::User {
            user_id: UserId::mint(),
        };
        for id in ["ev1_a", "ev1_a-1", "ev1_b"] {
            store
                .put_ownership(&Ownership {
                    client_id: ClientId::try_new(id)?,
                    owner: owner.clone(),
                })
                .await?;
        }
        let listed: Vec<String> = store
            .list_ownerships()
            .await?
            .into_iter()
            .map(|ownership| ownership.client_id.as_str().to_string())
            .collect();
        assert_eq!(listed, ["ev1_a-1", "ev1_a", "ev1_b"]);
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sql_racing_claims_name_the_winner() -> anyhow::Result<()> {
        const ROUNDS: usize = 20;
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(SqlOrgStore::new(test_db.db().clone()));
        for round in 0..ROUNDS {
            let slug = format!("race-{round}");
            let claims = race(|_| {
                let (store, slug) = (store.clone(), slug.clone());
                async move {
                    let org_id = OrgId::mint();
                    let claim = store.claim_org_slug(&slug, &org_id).await?;
                    Ok((org_id, claim))
                }
            })
            .await?;
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
            assert_eq!(
                store.lookup_org_by_slug(&slug).await?.as_ref(),
                winners.first().copied()
            );
        }
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM org_slug_index")
            .fetch_one(test_db.db().pool())
            .await?;
        assert_eq!(usize::try_from(count)?, ROUNDS, "one claim per handle");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn sql_racing_membership_puts_all_land() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let store = Arc::new(SqlOrgStore::new(test_db.db().clone()));
        let user_id = UserId::mint();
        let orgs: Vec<OrgId> = (0..RACERS).map(|_| OrgId::mint()).collect();
        race(|n| {
            let store = store.clone();
            let membership = Membership {
                user_id: user_id.clone(),
                org_id: orgs[n].clone(),
            };
            async move { store.put_membership(&membership).await }
        })
        .await?;
        let held = store.get_membership(&user_id).await?;
        assert!(held.is_some_and(|org_id| orgs.contains(&org_id)));
        assert_eq!(store.list_memberships().await?.len(), 1);
        Ok(())
    }
}
