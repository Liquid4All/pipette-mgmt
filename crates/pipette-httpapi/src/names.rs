//! Readable names for ids a body already shows. Nothing here takes an id from
//! the caller, so no route becomes an id-to-name oracle; a name that fails to
//! read leaves its id standing in.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use futures::stream;
use futures::stream::StreamExt;

use pipette_pipeline::descriptor_names::{CatalogReadScope, digest_displays};
use pipette_plan_types::{
    CampaignId, ClientId, DescriptorDisplay, PrincipalOwner, UserId, UserPermission,
    VaultResourceId,
};
use pipette_store::STORAGE_CONCURRENCY;

use crate::auth::{Caller, user_org};
use crate::campaign_access::CampaignReach;
use crate::error::AppError;
use crate::state::AppState;
use crate::vault_access::VaultReach;

/// The name of each distinct owner of `owners`, keyed by its id.
pub async fn owner_labels<'a>(
    state: &AppState,
    owners: impl Iterator<Item = &'a PrincipalOwner>,
) -> BTreeMap<String, String> {
    let distinct: HashSet<PrincipalOwner> = owners.cloned().collect();
    stream::iter(distinct)
        .map(|owner| async move {
            let read = match &owner {
                PrincipalOwner::User { user_id } => state
                    .user_store
                    .get_user(user_id)
                    .await
                    .map(|user| user.map(|user| user.contact_email.to_string())),
                PrincipalOwner::Org { org_id } => state
                    .org_store
                    .get_org(org_id)
                    .await
                    .map(|org| org.map(|org| org.slug.to_string())),
            };
            named(owner.label(), read)
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// The address of each distinct account of `submitters`, keyed by its id.
pub async fn submitter_addresses(
    state: &AppState,
    submitters: impl Iterator<Item = UserId>,
) -> BTreeMap<String, String> {
    let distinct: HashSet<UserId> = submitters.collect();
    stream::iter(distinct)
        .map(|user_id| async move {
            let read = state
                .user_store
                .get_user(&user_id)
                .await
                .map(|user| user.map(|user| user.contact_email.to_string()));
            named(user_id.to_string(), read)
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// The name of each distinct device of `client_ids`, keyed by its id. A job
/// names its device only for a reader holding `read_clients`.
pub async fn device_names(
    state: &AppState,
    client_ids: impl Iterator<Item = ClientId>,
) -> BTreeMap<String, String> {
    let distinct: Vec<ClientId> = client_ids.collect::<HashSet<_>>().into_iter().collect();
    state
        .client_store
        .get_clients(&distinct)
        .await
        .inspect_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "the devices' names did not read; their ids stand in");
        })
        .unwrap_or_default()
        .into_iter()
        .map(|client| {
            let name = device_name(client.client_details.as_str()).to_owned();
            (client.client_id.to_string(), name)
        })
        .collect()
}

/// The name of each vault resource of `resource_ids`, keyed by its id. A name
/// says more than the id a descriptor shows, so only `reach`'s are named.
pub async fn vault_names(
    state: &AppState,
    reach: &VaultReach,
    resource_ids: HashSet<VaultResourceId>,
) -> BTreeMap<String, String> {
    let distinct: Vec<VaultResourceId> = resource_ids.into_iter().collect();
    state
        .vault_store
        .get_resources(&distinct)
        .await
        .inspect_err(|e| {
            tracing::warn!(error = %format!("{e:#}"), "the vault names did not read; their ids stand in");
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|resource| reach.admits(&resource.owner))
        .map(|resource| (resource.resource_id.to_string(), resource.name.to_string()))
        .collect()
}

/// The name of each campaign of `campaign_ids` within `reach`, keyed by its id;
/// one outside it stays an id, as the campaign listing leaves it out.
pub async fn campaign_names(
    state: &AppState,
    reach: &CampaignReach,
    campaign_ids: HashSet<CampaignId>,
) -> BTreeMap<String, String> {
    stream::iter(campaign_ids)
        .map(|campaign_id| async move {
            let read = state
                .campaign_store
                .get_campaign(&campaign_id)
                .await
                .map(|campaign| {
                    campaign
                        .filter(|campaign| reach.admits(campaign))
                        .map(|campaign| campaign.name.to_string())
                });
            named(campaign_id.to_string(), read)
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .filter_map(std::future::ready)
        .collect()
        .await
}

/// The catalog scopes that the caller reads. The caller reads the global scope,
/// and the scope of the organization that it belongs to. A holder of
/// `edit_any_catalog` reads every organization.
pub async fn catalog_read_scope(
    state: &AppState,
    caller: Caller<'_>,
) -> Result<CatalogReadScope, AppError> {
    let own_org = user_org(state, &caller.user.user_id).await?;
    Ok(if caller.rights.holds(UserPermission::EditAnyCatalog) {
        CatalogReadScope::EveryOrg { own_org }
    } else {
        match own_org {
            Some(org_id) => CatalogReadScope::Org(org_id),
            None => CatalogReadScope::Unaffiliated,
        }
    })
}

/// The catalog names of each of `digests` that the caller can read, keyed by
/// the digest. The map is `None` without a `[database]`, and when no digest
/// has a name. A catalog that fails to read also gives `None`, and each row
/// shows its descriptor.
pub async fn descriptor_names(
    state: &AppState,
    caller: Caller<'_>,
    digests: BTreeSet<String>,
) -> Option<BTreeMap<String, DescriptorDisplay>> {
    let catalog = state.descriptor_catalog_store.as_deref()?;
    if digests.is_empty() {
        return None;
    }
    let digests: Vec<String> = digests.into_iter().collect();
    let read = async {
        let scope = catalog_read_scope(state, caller)
            .await
            .map_err(anyhow::Error::from)?;
        digest_displays(catalog, state.org_store.as_ref(), scope, &digests).await
    };
    match read.await {
        Ok(names) => {
            tracing::debug!(
                asked = digests.len(),
                named = names.len(),
                "descriptor names read for the digests that a body shows"
            );
            Some(names).filter(|names| !names.is_empty())
        }
        Err(e) => {
            tracing::warn!(
                asked = digests.len(),
                error = %format!("{e:#}"),
                "descriptor names did not read; each row shows its descriptor"
            );
            None
        }
    }
}

/// The `user@host` word of a device's details, which is how an operator reaches
/// a desktop, or the details whole for a device with none.
fn device_name(details: &str) -> &str {
    details
        .split_whitespace()
        .find(|word| word.contains('@'))
        .unwrap_or(details)
}

/// `id` and its name, or nothing for a record that is gone or did not read.
fn named(id: String, read: anyhow::Result<Option<String>>) -> Option<(String, String)> {
    match read {
        Ok(name) => name.map(|name| (id, name)),
        Err(e) => {
            tracing::warn!(id, error = %format!("{e:#}"), "a name did not read; the id stands in");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::device_name;

    #[rstest::rstest]
    #[case::ssh_address("Lab ops@lab-zenbook-1", "ops@lab-zenbook-1")]
    #[case::label("lab-s26-ultra - 2", "lab-s26-ultra - 2")]
    fn a_device_is_named_by_its_ssh_address_or_its_details(
        #[case] details: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(device_name(details), expected);
    }
}
