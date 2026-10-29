//! Fixtures shared by both stores' tests, and the assertions that span them:
//! what the two stores owe each other.

use chrono::Utc;

use pipette_core::client::Client;
use pipette_core::client_permission::ClientPermissionSet;
use pipette_core::types::Mint;
use pipette_core::user_permission::UserPermissionSet;
use pipette_plan_types::{ClientId, UserId};
use pipette_store::{ClientStore, KeyIndexStore, UserStore};

use crate::{LocalFsClientStore, LocalFsKeyIndexStore, LocalFsUserStore};

pub(super) fn client_store(dir: &tempfile::TempDir) -> anyhow::Result<LocalFsClientStore> {
    std::fs::create_dir_all(dir.path().join("clients"))?;
    Ok(LocalFsClientStore::new(dir.path().to_path_buf()))
}

pub(super) fn user_store(dir: &tempfile::TempDir) -> LocalFsUserStore {
    LocalFsUserStore::new(dir.path().to_path_buf())
}

pub(super) fn key_index_store(dir: &tempfile::TempDir) -> LocalFsKeyIndexStore {
    LocalFsKeyIndexStore::new(dir.path().to_path_buf())
}

pub(super) fn auth_test_client(id: &str, pk_suffix: &str, org: &str) -> anyhow::Result<Client> {
    // 64-char hex pubkey with a distinct 2-char suffix so clients differ.
    let public_key = pipette_plan_types::PublicKeyHex::try_new(format!(
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
    ))?;
    Ok(Client {
        client_id: ClientId::try_new(id)?,
        public_key,
        organization: pipette_plan_types::NonEmptyTrimmedString::try_new(org)?,
        client_details: pipette_plan_types::NonEmptyTrimmedString::try_new("details")?,
        contact_email: pipette_plan_types::ContactEmail::try_new("a@b.com")?,
        status: pipette_core::client::ClientStatus::Approved,
        registered_at: Utc::now(),
        device_profile: Default::default(),
        capabilities: Default::default(),
    })
}

/// The user pre-auth surface end to end, and that it shares no keyspace with
/// the client keys.
#[tokio::test]
async fn test_localfs_user_preauth_store() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    pipette_store_testkit::assert_user_preauth_store_contract(
        &user_store(&dir),
        &client_store(&dir)?,
    )
    .await
}

/// An account is not a client, and neither listing returns the other.
#[tokio::test]
async fn test_localfs_users_are_not_clients() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;

    let client = auth_test_client("ev1_fleet", "02", "acme")?;
    pipette_store_testkit::assert_users_are_not_clients(
        &client_store(&dir)?,
        &user_store(&dir),
        &client,
    )
    .await
}

/// Each path comes from one field, so only an assertion on the literal path
/// catches a prefix that moved.
#[tokio::test]
async fn test_localfs_permission_paths() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clients = client_store(&dir)?;
    let users = user_store(&dir);

    let client_id = ClientId::try_new("ev1_placed")?;
    let user_id = UserId::mint();
    clients
        .put_client_permissions(
            &client_id,
            &ClientPermissionSet::from_iter([pipette_plan_types::ClientPermission::PerformWork]),
        )
        .await?;
    users
        .put_user_permissions(
            &user_id,
            &UserPermissionSet::from_iter([pipette_plan_types::UserPermission::SubmitPlans]),
        )
        .await?;

    assert!(
        dir.path()
            .join(format!("client-permissions/{client_id}.json"))
            .exists(),
        "a client's set must land under client-permissions/"
    );
    assert!(
        dir.path()
            .join(format!("user-permissions/{user_id}.json"))
            .exists(),
        "a user's set must land under user-permissions/"
    );
    assert!(
        !dir.path().join("permissions").exists(),
        "neither kind may fall back to the directory the two once shared"
    );
    Ok(())
}

/// Each kind has its own directory, so neither write reaches the other's
/// object — and neither delete does.
#[tokio::test]
async fn test_localfs_permission_objects_are_per_principal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clients = client_store(&dir)?;

    let client = auth_test_client("ev1_shared", "01", "acme")?;
    clients.put_client(&client).await?;

    pipette_store_testkit::assert_permission_objects_are_per_principal(
        &clients,
        &user_store(&dir),
        &client.client_id,
        &UserId::mint(),
    )
    .await
}

/// Read as a set, a leftover `{client_id}.{uuid}.staged` file would count as a
/// client that does not exist.
#[tokio::test]
async fn test_localfs_permission_listing_skips_a_staged_file() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clients = client_store(&dir)?;
    let client = auth_test_client("ev1_listed", "02", "acme")?;
    let granted =
        ClientPermissionSet::from_iter([pipette_plan_types::ClientPermission::PerformWork]);
    clients.put_client(&client).await?;
    clients
        .put_client_permissions(&client.client_id, &granted)
        .await?;
    std::fs::write(
        dir.path()
            .join("client-permissions")
            .join("ev1_listed.4f1c.staged"),
        b"{\"permissions\":[\"perform_work\"]}",
    )?;

    assert_eq!(
        clients.list_client_permissions().await?,
        vec![(client.client_id, granted)]
    );
    Ok(())
}

/// Unlike a `.staged` file, this object is named as a set, so failing to read
/// it is no evidence that nobody granted anything. A directory in its place is
/// the portable read failure: `std::fs::read` returns `EISDIR`, not `NotFound`.
#[tokio::test]
async fn test_localfs_permission_listing_fails_on_an_unreadable_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clients = client_store(&dir)?;
    let client = auth_test_client("ev1_listed", "02", "acme")?;
    clients.put_client(&client).await?;
    clients
        .put_client_permissions(
            &client.client_id,
            &ClientPermissionSet::from_iter([pipette_plan_types::ClientPermission::PerformWork]),
        )
        .await?;
    std::fs::create_dir(
        dir.path()
            .join("client-permissions")
            .join("ev1_unreadable.json"),
    )?;

    assert!(
        clients.list_client_permissions().await.is_err(),
        "an unreadable permission object must fail the listing, not read as an empty set"
    );
    Ok(())
}

/// One layout, two directories — so neither listing may return the other's
/// keys.
#[tokio::test]
async fn test_localfs_preauth_keyspaces_do_not_cross() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let clients = LocalFsClientStore::new(dir.path().to_path_buf());
    let users = LocalFsUserStore::new(dir.path().to_path_buf());

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
        Utc::now(),
    )?;
    let user_key = pipette_core::user_preauth::mint(
        pipette_core::user_preauth::UserPreauthParams {
            user_id: UserId::mint(),
            usage: pipette_plan_types::UserPreauthUsage::SingleUse,
            expires_at: None,
            note: None,
        },
        Utc::now(),
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
        vec![user_key.key.key_id],
        "a user's listing must hold only a user's keys"
    );

    // Deleting one kind's record leaves the other's standing.
    clients
        .delete_client_preauth_key(&client_key.key.key_id)
        .await?;
    assert!(clients.list_client_preauth_keys().await?.is_empty());
    assert_eq!(users.list_user_preauth_keys().await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn test_localfs_user_keys_round_trip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    pipette_store_testkit::assert_user_key_store_roundtrip(
        &user_store(&dir),
        &UserId::mint(),
        &UserId::mint(),
    )
    .await
}

#[tokio::test]
async fn test_localfs_key_index_round_trips() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    pipette_store_testkit::assert_key_index_store_roundtrip(
        &key_index_store(&dir),
        &ClientId::try_new("ev1_claimant")?,
        &UserId::mint(),
    )
    .await
}

/// Each path comes from one field, so only an assertion on the literal path
/// catches a prefix that moved.
#[tokio::test]
async fn test_localfs_user_key_and_claim_paths() -> anyhow::Result<()> {
    use pipette_core::key_index::{KeyHolderPrincipalId, digest_of};
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PublicKeyHex;

    let dir = tempfile::tempdir()?;
    let users = user_store(&dir);
    let index = key_index_store(&dir);

    let user_id = UserId::mint();
    let public_key = PublicKeyHex::try_new(hex::encode([0x44; PublicKeyHex::BYTE_LEN]))?;
    let key = pipette_core::user::UserKey::new(
        public_key.clone(),
        NonEmptyTrimmedString::try_new("laptop")?,
        Utc::now(),
    );
    users.put_user_key(&user_id, &key).await?;

    let digest = digest_of(&public_key);
    index
        .claim_public_key(&digest, &KeyHolderPrincipalId::User(user_id.clone()))
        .await?;

    assert!(
        dir.path()
            .join(format!("user-keys/{user_id}/{}.json", key.key_id))
            .exists(),
        "a registration must land under its account's key directory"
    );
    assert!(
        dir.path()
            .join(format!("public-key-index/{digest}"))
            .exists(),
        "a claim must land under public-key-index/ keyed by the bare digest"
    );
    Ok(())
}
