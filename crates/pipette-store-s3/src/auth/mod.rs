//! The auth keyspaces on an object store: the record prefixes, the layout
//! modules that address them, and the two stores over the top.

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

pub use client_store::S3ClientStore;
pub(crate) use key_index_store::S3KeyIndexStore;
pub use user_store::S3UserStore;
