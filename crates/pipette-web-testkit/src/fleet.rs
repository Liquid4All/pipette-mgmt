//! A seeded fleet with a known shape, so the JSON route and the scheduler are
//! compared over one roster rather than two.

use anyhow::Context;
use ed25519_dalek::SigningKey;
use serde_json::Value;

use pipette_httpapi::state::AppState;
use pipette_plan_types::{Tag, UserId, UserKeyId, UserPermission};

use crate::{make_state, registered_user, setup_benchmarks};

/// Stable keys put nonmatches between and after matches during cursor tests.
async fn seed_filter_fleet(state: &AppState) -> anyhow::Result<()> {
    use pipette_core::client::{Client, ClientStatus};
    use pipette_plan_types::ClientId;
    use pipette_plan_types::ContactEmail;
    use pipette_plan_types::DeviceProfile;
    use pipette_plan_types::NonEmptyTrimmedString;
    use pipette_plan_types::PublicKeyHex;

    for (index, (id, os, gpu, runtime, tags, status)) in [
        (
            "c-a",
            Some("iOS"),
            Some("A17 Pro"),
            "runtime:llama_cpp",
            &["lab", "us-east"][..],
            ClientStatus::Approved,
        ),
        (
            "c-b",
            Some("Android"),
            None,
            "runtime:llama_cpp",
            &["lab"][..],
            ClientStatus::Approved,
        ),
        (
            "c-c",
            Some("iOS"),
            Some("A18 Pro"),
            "runtime:other",
            &["lab", "us-east"][..],
            ClientStatus::Pending,
        ),
        (
            "c-d",
            None,
            None,
            "runtime:other",
            &[][..],
            ClientStatus::Approved,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let client = Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(format!("{index:064x}"))?,
            organization: NonEmptyTrimmedString::try_new("test-org")?,
            client_details: NonEmptyTrimmedString::try_new("test")?,
            contact_email: ContactEmail::try_new("operator@example.com")?,
            status,
            registered_at: chrono::Utc::now(),
            device_profile: DeviceProfile {
                device_name: os
                    .map(|_| NonEmptyTrimmedString::try_new("iPhone14,2"))
                    .transpose()?,
                device_os_name: os.map(NonEmptyTrimmedString::try_new).transpose()?,
                device_gpu_model: gpu.map(NonEmptyTrimmedString::try_new).transpose()?,
                ..Default::default()
            },
            capabilities: [runtime.to_string()].into_iter().collect(),
        };
        state.client_store.put_client(&client).await?;
        for tag in tags {
            state
                .tag_store
                .add_client_tag(&client.client_id, &Tag::try_new(*tag)?)
                .await?;
        }
    }
    Ok(())
}

/// The seeded fleet over `dir`, and an account that may read it.
///
/// The caller owns `dir`, for the reason [`crate::approved_client`] gives.
pub async fn filter_fleet(
    dir: &std::path::Path,
) -> anyhow::Result<(AppState, SigningKey, UserId, UserKeyId)> {
    setup_benchmarks(dir)?;
    let state = make_state(dir).await?;
    seed_filter_fleet(&state).await?;
    let (key, user_id, key_id) = registered_user(&state, &[UserPermission::ReadClients]).await?;
    Ok((state, key, user_id, key_id))
}

pub fn fleet_ids(body: &Value) -> anyhow::Result<Vec<&str>> {
    body["items"]
        .as_array()
        .context("a listing carries an `items` array")?
        .iter()
        .map(|row| {
            row["client_id"]
                .as_str()
                .context("a row carries a client id")
        })
        .collect()
}
