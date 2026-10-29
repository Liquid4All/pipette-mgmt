//! The HTTP API: the route table ([`router::app`], which `serve` and the
//! integration tests both build) and every route behind it, the device protocol
//! in [`client`] and the operator API in [`api_v1`].

#![cfg_attr(
    test,
    expect(
        clippy::disallowed_macros,
        clippy::disallowed_methods,
        reason = "tests build the requests a client sends, so they keep `json!` (see clippy.toml)"
    )
)]

pub mod api_schema;
pub mod api_v1;
pub mod audit;
pub mod auth;
pub mod campaign_access;
pub mod clerk;
pub mod client;
pub mod error;
mod etag;
pub mod extract;
pub mod gate;
pub mod health;
pub mod list;
pub mod names;
pub mod owner;
pub mod owner_choices;
mod progress_stream;
pub mod registration_throttle;
mod router;
mod serve;
pub mod state;
pub mod ui;
pub mod vault_access;

pub use router::app;
pub use serve::{Limits, serve};
pub use state::AppState;
