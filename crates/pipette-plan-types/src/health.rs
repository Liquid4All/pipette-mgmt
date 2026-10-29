//! `GET /health`.

use serde::{Deserialize, Serialize};

/// A fixed liveness marker: `{"status": "ok"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
}
