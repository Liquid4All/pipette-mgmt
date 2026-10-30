//! [`SqlKeyIndexStore`] keeps `public-key-index/` as a table with no foreign
//! key: a claim names a client or a user and precedes the record it protects.

use anyhow::Context;
use async_trait::async_trait;

use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest};
use pipette_store::KeyIndexStore;

use crate::ClaimVanishedError;
use crate::db::SqlDatabase;
use crate::retry::retry;

#[derive(Clone)]
pub(crate) struct SqlKeyIndexStore {
    db: SqlDatabase,
}

impl SqlKeyIndexStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }
}

/// A corrupt claim is an error. If the store read the claim as unclaimed, the
/// next caller would get the keypair.
fn parse_holder(digest: &PublicKeyDigest, holder: &str) -> anyhow::Result<KeyHolderPrincipalId> {
    holder
        .trim()
        .parse()
        .with_context(|| format!("the public-key claim for {digest} names no principal"))
}

#[async_trait]
impl KeyIndexStore for SqlKeyIndexStore {
    /// The incumbent is read in the insert's snapshot, so no release lands
    /// between them. A racing duplicate conflicts; its retry reads the winner.
    async fn claim_public_key(
        &self,
        digest: &PublicKeyDigest,
        holder: &KeyHolderPrincipalId,
    ) -> anyhow::Result<KeyClaim> {
        let db = &self.db;
        let key = digest.as_str();
        let claimant = holder.as_str();
        let incumbent = retry("claim_public_key", move || {
            Box::pin(async move {
                let mut tx = db.begin().await?;
                let created = sqlx::query(
                    "INSERT INTO public_key_index (digest, holder) VALUES ($1, $2)
                     ON CONFLICT (digest) DO NOTHING",
                )
                .bind(key)
                .bind(claimant)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if created != 0 {
                    tx.commit().await?;
                    return Ok(None);
                }
                let incumbent: Option<String> =
                    sqlx::query_scalar("SELECT holder FROM public_key_index WHERE digest = $1")
                        .bind(key)
                        .fetch_optional(&mut *tx)
                        .await?;
                Ok(Some(incumbent))
            })
        })
        .await?;
        match incumbent {
            None => Ok(KeyClaim::Claimed),
            Some(Some(incumbent)) => Ok(KeyClaim::Taken(parse_holder(digest, &incumbent)?)),
            Some(None) => Err(ClaimVanishedError {
                claim: "public-key",
                name: digest.to_string(),
            }
            .into()),
        }
    }

    async fn holder_of_public_key(
        &self,
        digest: &PublicKeyDigest,
    ) -> anyhow::Result<Option<KeyHolderPrincipalId>> {
        let holder: Option<(String,)> = self
            .db
            .fetch_optional("holder_of_public_key", || {
                sqlx::query_as("SELECT holder FROM public_key_index WHERE digest = $1")
                    .bind(digest.as_str())
            })
            .await?;
        holder
            .map(|(holder,)| parse_holder(digest, &holder))
            .transpose()
    }

    async fn release_public_key(&self, digest: &PublicKeyDigest) -> anyhow::Result<()> {
        self.db
            .execute("release_public_key", || {
                sqlx::query("DELETE FROM public_key_index WHERE digest = $1").bind(digest.as_str())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest, digest_of};
    use pipette_core::types::Mint;
    use pipette_plan_types::{ClientId, UserId};
    use pipette_store::KeyIndexStore;

    use super::SqlKeyIndexStore;
    use crate::db::SqlDatabase;
    use crate::testing::{public_key, race};

    fn digest(byte: u8) -> anyhow::Result<PublicKeyDigest> {
        Ok(digest_of(&public_key(byte)?))
    }

    async fn write_claim(
        db: &SqlDatabase,
        digest: &PublicKeyDigest,
        holder: &str,
    ) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO public_key_index (digest, holder) VALUES ($1, $2)")
            .bind(digest.as_str())
            .bind(holder)
            .execute(db.pool())
            .await?;
        Ok(())
    }

    #[tokio::test]
    async fn sql_key_index_round_trips() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        pipette_store_testkit::assert_key_index_store_roundtrip(
            &SqlKeyIndexStore::new(test_db.db().clone()),
            &ClientId::try_new("ev1_claimant")?,
            &UserId::mint(),
        )
        .await
    }

    #[tokio::test]
    async fn a_corrupt_claim_is_an_error_not_a_free_keypair() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let claims = SqlKeyIndexStore::new(db.clone());
        let digest = digest(0x55)?;
        write_claim(db, &digest, "not-a-principal").await?;
        let claimant = KeyHolderPrincipalId::User(UserId::mint());

        assert!(claims.holder_of_public_key(&digest).await.is_err());
        assert!(claims.claim_public_key(&digest, &claimant).await.is_err());

        claims.release_public_key(&digest).await?;
        assert_eq!(
            claims.claim_public_key(&digest, &claimant).await?,
            KeyClaim::Claimed
        );
        Ok(())
    }

    /// The holder is read trimmed, as the object stores read a claim body.
    #[tokio::test]
    async fn a_claim_body_is_read_trimmed() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let db = test_db.db();
        let claims = SqlKeyIndexStore::new(db.clone());
        let digest = digest(0x66)?;
        write_claim(db, &digest, "ev1_padded\n").await?;
        assert_eq!(
            claims.holder_of_public_key(&digest).await?,
            Some(KeyHolderPrincipalId::Client(ClientId::try_new(
                "ev1_padded"
            )?))
        );
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn racing_claims_have_one_winner() -> anyhow::Result<()> {
        let test_db = crate::testing::test_db_or_skip!();
        let claims = SqlKeyIndexStore::new(test_db.db().clone());
        let digest = digest(0x77)?;
        let outcomes = race(|nth| {
            let (claims, digest) = (claims.clone(), digest.clone());
            async move {
                let holder =
                    KeyHolderPrincipalId::Client(ClientId::try_new(format!("ev1_racer-{nth}"))?);
                let claim = claims.claim_public_key(&digest, &holder).await?;
                Ok((holder, claim))
            }
        })
        .await?;

        let winners: Vec<_> = outcomes
            .iter()
            .filter(|(_, claim)| *claim == KeyClaim::Claimed)
            .map(|(holder, _)| holder.clone())
            .collect();
        let [winner] = winners.as_slice() else {
            anyhow::bail!("exactly one claim must land, got {outcomes:?}");
        };
        assert!(
            outcomes
                .iter()
                .all(|(_, claim)| *claim == KeyClaim::Claimed || claim.is_held_by(winner)),
            "every loser is told who won: {outcomes:?}"
        );
        assert_eq!(
            claims.holder_of_public_key(&digest).await?.as_ref(),
            Some(winner)
        );
        Ok(())
    }
}
