//! The repeatable filters of `GET /api/v1/clients`, read from the raw query
//! pairs: serde's struct derive reads one value per key.

use pipette_pipeline::client_cli;
use pipette_pipeline::client_cli::CapabilityQuery;
use pipette_plan_types::Tag;

use crate::error::AppError;

/// A repeatable fleet filter key, as it is spelled in a query string.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::EnumIter,
)]
#[strum(serialize_all = "snake_case")]
pub enum FleetFilterKey {
    /// One occurrence per tag; a client must carry all of them.
    Tag,
    Requires,
    AnyOf,
    Excludes,
    NotAllOf,
}

/// The repeatable filter parameters of `GET /api/v1/clients`.
#[derive(Default)]
pub struct FleetApiFilters {
    pub tags: Vec<Tag>,
    pub capabilities: CapabilityQuery,
}

impl FleetApiFilters {
    /// Parse the repeatable filter keys from raw query pairs, ignoring every key
    /// [`FleetFilterKey`] does not name.
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Self, AppError> {
        Self::from_occurrences(pairs.iter().filter_map(|(key, value)| {
            key.parse::<FleetFilterKey>()
                .ok()
                .map(|key| (key, value.as_str()))
        }))
    }

    /// Parse filter occurrences already resolved to a key, in submission order.
    /// `any_of` and `not_all_of` are a clause per occurrence, where `requires`
    /// and `excludes` are one set across all of theirs.
    pub fn from_occurrences<'a>(
        occurrences: impl IntoIterator<Item = (FleetFilterKey, &'a str)>,
    ) -> Result<Self, AppError> {
        let mut filters = occurrences.into_iter().try_fold(
            Self::default(),
            |mut filters, (key, value)| -> Result<Self, AppError> {
                match key {
                    FleetFilterKey::Tag => filters.tags.push(Tag::try_new(value.to_string())?),
                    FleetFilterKey::Requires => filters
                        .capabilities
                        .requires
                        .extend(client_cli::flag_list(value)?),
                    FleetFilterKey::Excludes => filters
                        .capabilities
                        .excludes
                        .extend(client_cli::flag_list(value)?),
                    FleetFilterKey::AnyOf => filters
                        .capabilities
                        .any_of
                        .push(client_cli::flag_list(value)?),
                    FleetFilterKey::NotAllOf => filters
                        .capabilities
                        .not_all_of
                        .push(client_cli::flag_list(value)?),
                }
                Ok(filters)
            },
        )?;
        // One listing per distinct tag: a repeated occurrence adds nothing to
        // the intersection.
        filters.tags.sort();
        filters.tags.dedup();
        Ok(filters)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The five words `docs/api/devices/list_clients.md` publishes, spelled
    /// out, so dropping `serialize_all` cannot silence every client that sends
    /// `any_of`.
    #[test]
    fn the_filter_keys_are_the_published_words() {
        assert_eq!(
            <FleetFilterKey as strum::IntoEnumIterator>::iter()
                .map(<&'static str>::from)
                .collect::<Vec<_>>(),
            ["tag", "requires", "any_of", "excludes", "not_all_of"]
        );
    }
}
