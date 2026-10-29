//! Fixtures shared by both stores' tests, and the assertions that span them:
//! what the two stores owe each other.

use std::sync::Arc;

use anyhow::Context;
use chrono::TimeZone;
use object_store::memory::InMemory;
use object_store::path::Path as ObjPath;
use object_store::{ObjectStore, ObjectStoreExt};

use pipette_core::client::Client;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::types::Mint;
use pipette_core::user_permission::UserPermissionSet;
use pipette_plan_types::{ClientId, UserId};
use pipette_store::{ClientStore, KeyIndexStore, UserStore};

use super::*;

pub(super) fn client_store() -> (Arc<dyn ObjectStore>, S3ClientStore) {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let client_store = S3ClientStore::new(store.clone(), "test-prefix".to_string());
    (store, client_store)
}

pub(super) fn user_store() -> (Arc<dyn ObjectStore>, S3UserStore) {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let user_store = S3UserStore::new(store.clone(), "test-prefix".to_string());
    (store, user_store)
}

/// Both stores over *one* backend, for the assertions that span them.
pub(super) fn both_stores() -> (Arc<dyn ObjectStore>, S3ClientStore, S3UserStore) {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    (
        store.clone(),
        S3ClientStore::new(store.clone(), "test-prefix".to_string()),
        S3UserStore::new(store, "test-prefix".to_string()),
    )
}

pub(super) fn test_client(
    client_id: &str,
    public_key: &str,
    registered_at_secs: i64,
) -> anyhow::Result<Client> {
    Ok(Client {
        client_id: ClientId::try_new(client_id)?,
        public_key: pipette_plan_types::PublicKeyHex::try_new(public_key)?,
        organization: pipette_plan_types::NonEmptyTrimmedString::try_new("test-org")?,
        client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("details")?,
        contact_email: pipette_plan_types::ContactEmail::try_new("a@b.com")?,
        status: pipette_core::client::ClientStatus::Pending,
        registered_at: chrono::Utc
            .timestamp_opt(registered_at_secs, 0)
            .single()
            .context("valid timestamp")?,
        device_profile: Default::default(),
        capabilities: Default::default(),
    })
}

/// The user pre-auth surface end to end, and that it shares no keyspace with
/// the client keys.
#[tokio::test]
async fn s3_user_preauth_store() -> anyhow::Result<()> {
    let (_store, clients, users) = both_stores();

    pipette_store_testkit::assert_user_preauth_store_contract(&users, &clients).await
}

/// An account is not a client, and neither listing returns the other.
#[tokio::test]
async fn s3_users_are_not_clients() -> anyhow::Result<()> {
    let (_store, clients, users) = both_stores();

    let client = test_client(
        "ev1_fleet",
        "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
        0,
    )?;
    pipette_store_testkit::assert_users_are_not_clients(&clients, &users, &client).await
}

/// Each key comes from one keyspace constant, so only an assertion on the
/// literal key catches a prefix that moved.
#[tokio::test]
async fn s3_permission_objects_use_the_per_kind_prefixes() -> anyhow::Result<()> {
    use pipette_plan_types::ClientPermission;
    use pipette_plan_types::UserPermission;

    let (store, clients, users) = both_stores();
    let client_id = ClientId::try_new("ev1_placed")?;
    let user_id = UserId::mint();

    clients
        .put_client_permissions(
            &client_id,
            &ClientPermissionSet::from_iter([ClientPermission::PerformWork]),
        )
        .await?;
    users
        .put_user_permissions(
            &user_id,
            &UserPermissionSet::from_iter([UserPermission::SubmitPlans]),
        )
        .await?;

    let client_key = ObjPath::from(format!("test-prefix/client-permissions/{client_id}.json"));
    let client_object = store
        .get(&client_key)
        .await
        .with_context(|| {
            format!("a client's set must land at client-permissions/{client_id}.json")
        })?
        .bytes()
        .await?;
    // The envelope too: a round trip would pass just as well if this backend
    // dropped the wrapper key at both ends.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&client_object)?,
        serde_json::json!({ "permissions": ["perform_work"] }),
        "a client's object must carry the stored envelope"
    );
    let user_key = ObjPath::from(format!("test-prefix/user-permissions/{user_id}.json"));
    let user_object = store
        .get(&user_key)
        .await
        .with_context(|| format!("a user's set must land at user-permissions/{user_id}.json"))?
        .bytes()
        .await?;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&user_object)?,
        serde_json::json!({ "permissions": ["submit_plans"] }),
        "a user's object must carry the stored envelope"
    );
    assert!(
        store
            .get(&ObjPath::from(format!(
                "test-prefix/permissions/{client_id}.json"
            )))
            .await
            .is_err(),
        "neither kind may fall back to the prefix the two once shared"
    );
    Ok(())
}

/// Each kind has its own prefix, so neither write reaches the other's
/// object — and neither delete does.
#[tokio::test]
async fn s3_permission_objects_are_per_principal() -> anyhow::Result<()> {
    let (_store, clients, users) = both_stores();

    pipette_store_testkit::assert_permission_objects_are_per_principal(
        &clients,
        &users,
        &ClientId::try_new("ev1_shared")?,
        &UserId::mint(),
    )
    .await
}

/// Neither listing may return the other kind's keys, and spending one must not
/// retire the other.
#[tokio::test]
async fn s3_preauth_keyspaces_do_not_cross() -> anyhow::Result<()> {
    let (_store, clients, users) = both_stores();

    let client_key = pipette_core::client_preauth::mint(
        pipette_core::client_preauth::ClientPreauthParams {
            usage: pipette_plan_types::ClientPreauthUsage::SingleUse,
            expires_at: None,
            default_tags: Default::default(),
            default_organization: None,
            default_owner: None,
            default_permissions: None,
            note: None,
        },
        chrono::Utc::now(),
    )?;
    let user_key = pipette_core::user_preauth::mint(
        pipette_core::user_preauth::UserPreauthParams {
            user_id: UserId::mint(),
            usage: pipette_plan_types::UserPreauthUsage::SingleUse,
            expires_at: None,
            note: None,
        },
        chrono::Utc::now(),
    )?;
    clients.put_client_preauth_key(&client_key.key).await?;
    users.put_user_preauth_key(&user_key.key).await?;

    assert_eq!(
        clients
            .list_client_preauth_keys()
            .await?
            .iter()
            .map(|key| key.key_id.clone())
            .collect::<Vec<_>>(),
        vec![client_key.key.key_id.clone()],
        "a client's listing must hold only a client's keys"
    );
    assert_eq!(
        users
            .list_user_preauth_keys()
            .await?
            .iter()
            .map(|key| key.key_id.clone())
            .collect::<Vec<_>>(),
        vec![user_key.key.key_id.clone()],
        "a user's listing must hold only a user's keys"
    );

    // Spending the user's key retires that key and nothing else.
    let (_, secret) = user_key
        .token
        .parse_parts()
        .context("the minted token must parse")?;
    assert!(matches!(
        users
            .consume_user_preauth_key(&user_key.key.key_id, &secret)
            .await?,
        pipette_core::user_preauth::UserPreauthConsumeOutcome::Granted(_)
    ));
    assert_eq!(
        clients.list_client_preauth_keys().await?.len(),
        1,
        "spending a user's key must leave a client's record standing"
    );
    assert!(
        clients
            .list_spent_client_preauth_markers()
            .await?
            .is_empty(),
        "the marker belongs to the keyspace that was spent"
    );
    Ok(())
}

#[tokio::test]
async fn s3_user_keys_round_trip() -> anyhow::Result<()> {
    let (_store, _clients, users) = both_stores();
    pipette_store_testkit::assert_user_key_store_roundtrip(&users, &UserId::mint(), &UserId::mint())
        .await
}

#[tokio::test]
async fn s3_key_index_round_trips() -> anyhow::Result<()> {
    let (store, _clients, _users) = both_stores();
    let index = S3KeyIndexStore::new(store, "test-prefix".to_string());
    pipette_store_testkit::assert_key_index_store_roundtrip(
        &index,
        &ClientId::try_new("ev1_claimant")?,
        &UserId::mint(),
    )
    .await
}

/// Each key comes from one keyspace constant, so only an assertion on the
/// literal key catches a prefix that moved.
#[tokio::test]
async fn s3_user_key_and_claim_objects_use_their_prefixes() -> anyhow::Result<()> {
    use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PublicKeyHex;

    let (store, _clients, users) = both_stores();
    let index = S3KeyIndexStore::new(store.clone(), "test-prefix".to_string());

    let user_id = UserId::mint();
    let public_key = PublicKeyHex::try_new(hex::encode([0x44; PublicKeyHex::BYTE_LEN]))?;
    let key = pipette_core::user::UserKey::new(
        public_key.clone(),
        NonEmptyTrimmedString::try_new("laptop")?,
        chrono::Utc::now(),
    );
    users.put_user_key(&user_id, &key).await?;

    let digest = digest_of(&public_key);
    index
        .claim_public_key(&digest, &KeyHolderPrincipalId::User(user_id.clone()))
        .await?;

    for expected in [
        format!("test-prefix/user-keys/{user_id}/{}.json", key.key_id),
        format!("test-prefix/public-key-index/{digest}"),
    ] {
        assert!(
            store
                .get(&object_store::path::Path::from(expected.as_str()))
                .await
                .is_ok(),
            "expected an object at {expected}"
        );
    }
    Ok(())
}
