//! What the client and user stores share for their permission sets.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// The `{"permissions": [...]}` envelope that the object stores use. A body
/// copied between backends reads the same.
#[derive(Serialize, Deserialize)]
struct PermissionsBody<S> {
    permissions: S,
}

pub(crate) fn encode_permissions<S: Serialize>(set: &S) -> serde_json::Result<String> {
    serde_json::to_string_pretty(&PermissionsBody { permissions: set })
}

pub(crate) fn decode_permissions<S: DeserializeOwned>(body: &str) -> serde_json::Result<S> {
    serde_json::from_str::<PermissionsBody<S>>(body).map(|body| body.permissions)
}

/// A mutation that failed because concurrent writers changed the same
/// permission set.
#[derive(Debug, thiserror::Error)]
#[error(
    "permissions for {principal_id} kept changing under concurrent read-modify-write \
     attempts; nothing was written"
)]
pub(crate) struct PermissionContentionError {
    pub(crate) principal_id: String,
}
