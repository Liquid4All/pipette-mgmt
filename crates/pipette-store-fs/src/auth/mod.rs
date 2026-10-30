//! The auth trees on a filesystem: the record directories, the layout modules
//! that address them, and the two stores over the top.

mod client_layout;
mod client_permission_layout;
mod client_store;
mod key_index_layout;
mod key_index_store;
mod preauth_layout;
mod user_layout;
mod user_permission_layout;
mod user_store;

#[cfg(test)]
mod tests;

pub use client_store::LocalFsClientStore;
pub(crate) use key_index_store::LocalFsKeyIndexStore;
pub use user_store::LocalFsUserStore;

use super::run_blocking;
