//! Client selection by capability query, and the fleet's capability flags,
//! shared by the command and the HTTP routes so the two cannot disagree.

use std::collections::{BTreeMap, BTreeSet};

use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};
use serde::Serialize;
use serde_json::Value;

use pipette_core::client::{Client, ClientStatus, is_reserved_capability, is_tag_capability};
use pipette_core::matching;
use pipette_core::matching::{FleetTags, NarrowingClauses};
use pipette_plan_types::{ClientId, Tag};
use pipette_store::{
    ClientStore, STORAGE_CONCURRENCY, TagStore, drain_clients, warn_unreadable_clients,
};

/// An invalid capability flag or flag list. The comparison is byte-exact.
#[derive(Debug, thiserror::Error)]
pub enum FlagError {
    #[error("the capability flag is empty")]
    Empty,
    #[error("{flag:?} is not canonical. Use {canonical:?}")]
    NotCanonical { flag: String, canonical: String },
    #[error("the capability flag list is empty")]
    EmptyList,
}

/// Validate a flag. Preserve its text. The comparison is byte-exact.
fn canonical_flag(s: &str) -> Result<String, FlagError> {
    let canonical = pipette_core::client::slugify(s);
    if canonical.is_empty() {
        return Err(FlagError::Empty);
    }
    if s != canonical {
        return Err(FlagError::NotCanonical {
            flag: s.to_string(),
            canonical,
        });
    }
    Ok(s.to_string())
}

/// Parse one nonempty list. Whitespace separates flags; commas can occur in flags.
pub fn flag_list(s: &str) -> Result<Vec<String>, FlagError> {
    let flags: Vec<String> = s
        .split_whitespace()
        .map(canonical_flag)
        .collect::<Result<_, _>>()?;
    if flags.is_empty() {
        return Err(FlagError::EmptyList);
    }
    Ok(flags)
}

/// The four flag clauses of a job body, grouped because two pairs share a type
/// and a swap would compile. The caller already names the client, so no
/// `clients` clause.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapabilityQuery {
    pub requires: Vec<String>,
    pub any_of: Vec<Vec<String>>,
    pub excludes: Vec<String>,
    pub not_all_of: Vec<Vec<String>>,
}

impl CapabilityQuery {
    pub fn is_present(&self) -> bool {
        !self.requires.is_empty()
            || !self.any_of.is_empty()
            || !self.excludes.is_empty()
            || !self.not_all_of.is_empty()
    }

    /// Clauses without `requires` match no client: the others only restrict.
    pub fn matches_nobody(&self) -> bool {
        self.requires.is_empty() && self.is_present()
    }

    /// The query as a job body for the scheduler predicate. It names no clients:
    /// the caller identifies each client before testing its flags.
    pub fn body(&self) -> Value {
        NarrowingClauses {
            requires: &self.requires,
            any_of: &self.any_of,
            excludes: &self.excludes,
            not_all_of: &self.not_all_of,
            ..Default::default()
        }
        .body()
    }
}

/// Caches one capability query body for use against multiple clients.
pub struct CapabilityMatcher {
    /// The job body. `None` when the query carries no clause.
    body: Option<Value>,
    /// The clients' tags, which the clauses read beside their capabilities.
    tags: FleetTags,
}

impl CapabilityMatcher {
    /// Prepare `query` for match tests. The fleet's tags are read only for a
    /// query with `requires`: any other admits every client or none.
    pub async fn load(query: &CapabilityQuery, tag_index: &dyn TagStore) -> anyhow::Result<Self> {
        let tags = if query.requires.is_empty() {
            FleetTags::default()
        } else {
            FleetTags::new(tag_index.list_forward_tag_markers().await?)
        };
        Ok(Self {
            body: query.is_present().then(|| query.body()),
            tags,
        })
    }

    /// Whether `client` passes the query. A query with no clause admits everyone.
    pub fn matches(&self, client: &Client) -> bool {
        self.body
            .as_ref()
            .is_none_or(|body| matching::job_matches_client(body, client, &self.tags))
    }
}

/// The client ids carrying every tag in `tags`, from the reverse index. An
/// empty `tags` gives an empty set, not the fleet.
pub async fn clients_with_all_tags(
    store: &dyn TagStore,
    tags: &[Tag],
) -> anyhow::Result<BTreeSet<ClientId>> {
    // Each future owns its tag: borrowing it across `buffer_unordered` breaks the
    // `Send` the HTTP handler needs.
    let sets: Vec<BTreeSet<ClientId>> = stream::iter(tags.iter().cloned())
        .map(|tag| async move {
            anyhow::Ok(
                store
                    .list_client_ids_by_tag(&tag)
                    .await?
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
            )
        })
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;
    Ok(sets
        .into_iter()
        .reduce(|a, b| a.intersection(&b).cloned().collect())
        .unwrap_or_default())
}

/// The clients passing both `clients list` filters, newest registration first.
/// An empty `tags` selects the whole fleet.
pub async fn select_clients(
    store: &dyn ClientStore,
    tag_index: &dyn TagStore,
    tags: &[Tag],
    query: &CapabilityQuery,
) -> anyhow::Result<Vec<Client>> {
    let mut clients: Vec<Client> = if tags.is_empty() {
        let fleet = drain_clients(store).await?;
        warn_unreadable_clients(&fleet.unreadable);
        fleet.items
    } else {
        let ids = clients_with_all_tags(tag_index, tags).await?;
        stream::iter(
            ids.into_iter()
                .map(|id| async move { anyhow::Ok(store.get_client(&id).await?) }),
        )
        .buffer_unordered(STORAGE_CONCURRENCY)
        .try_filter_map(|c| async move { anyhow::Ok(c) })
        .try_collect()
        .await?
    };
    clients.sort_by_key(|c| std::cmp::Reverse(c.registered_at));
    let matcher = CapabilityMatcher::load(query, tag_index).await?;
    clients.retain(|client| matcher.matches(client));
    Ok(clients)
}

/// The capability-flag vocabulary the fleet reports, grouped by namespace, for
/// a plan author writing a job's `requires`.
#[derive(Debug, Serialize)]
pub struct CapabilityVocabulary {
    /// Namespaced flags, one entry per namespace, ascending by namespace.
    pub namespaces: Vec<CapabilityNamespace>,
    /// Flags that carry no `:` at all, ascending.
    pub free_form: Vec<CapabilityFlagCount>,
    /// Number of client records that pass the status filter.
    pub clients: usize,
    /// Client records the scan could not read, so a reader can tell "no client
    /// reports this flag" from an incomplete scan.
    pub unreadable: usize,
}

/// A sorted group of flags that share one namespace and source.
#[derive(Debug, Serialize)]
pub struct CapabilityNamespace {
    pub namespace: String,
    pub source: CapabilitySource,
    pub flags: Vec<CapabilityFlagCount>,
}

#[derive(Debug, Serialize)]
pub struct CapabilityFlagCount {
    pub flag: String,
    /// Clients that carry this flag within the filtered set.
    pub clients: usize,
}

/// Where a flag comes from; `clients list` shows each source in its own column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, strum::IntoStaticStr)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum CapabilitySource {
    /// Normalized from the `device_*` profile, in a reserved namespace clients
    /// cannot report into.
    Derived,
    /// The client reported the flag directly, for example `runtime:llama_cpp`.
    Declared,
    /// An operator assigned the tag, `tag:<tag>`, through a pre-auth key or
    /// `clients tag add`.
    Assigned,
}

impl From<CapabilityVocabulary> for pipette_plan_types::CapabilityVocabulary {
    fn from(vocabulary: CapabilityVocabulary) -> Self {
        Self {
            namespaces: vocabulary.namespaces.into_iter().map(Into::into).collect(),
            free_form: vocabulary.free_form.into_iter().map(Into::into).collect(),
            client_count: vocabulary.clients,
            unreadable_client_count: vocabulary.unreadable,
        }
    }
}

impl From<CapabilityNamespace> for pipette_plan_types::CapabilityNamespace {
    fn from(namespace: CapabilityNamespace) -> Self {
        Self {
            namespace: namespace.namespace,
            source: match namespace.source {
                CapabilitySource::Derived => pipette_plan_types::CapabilitySource::Derived,
                CapabilitySource::Declared => pipette_plan_types::CapabilitySource::Declared,
                CapabilitySource::Assigned => pipette_plan_types::CapabilitySource::Assigned,
            },
            flags: namespace.flags.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<CapabilityFlagCount> for pipette_plan_types::CapabilityFlagCount {
    fn from(count: CapabilityFlagCount) -> Self {
        Self {
            flag: count.flag,
            client_count: count.clients,
        }
    }
}

/// The distinct capability flags among the clients `status` selects, counting
/// each flag the matcher tests (record flags and tags). A flag joins the group
/// its first `:` names. Lenient per record; it counts what it could not read.
pub async fn capability_vocabulary(
    store: &dyn ClientStore,
    tag_index: &dyn TagStore,
    status: Option<ClientStatus>,
) -> anyhow::Result<CapabilityVocabulary> {
    let fleet = drain_clients(store).await?;
    warn_unreadable_clients(&fleet.unreadable);
    let tags = FleetTags::new(tag_index.list_forward_tag_markers().await?);

    let selected: Vec<&Client> = fleet
        .items
        .iter()
        .filter(|client| status.is_none_or(|status| client.status == status))
        .collect();

    // The maps preserve the sort order of namespaces and flags.
    let counts: BTreeMap<String, usize> = selected
        .iter()
        .flat_map(|client| tags.capabilities_of(client))
        .fold(BTreeMap::new(), |mut counts, flag| {
            *counts.entry(flag).or_default() += 1;
            counts
        });

    let (grouped, free_form) = counts.into_iter().fold(
        (
            BTreeMap::<String, Vec<CapabilityFlagCount>>::new(),
            Vec::new(),
        ),
        |(mut grouped, mut free_form), (flag, clients)| {
            match flag
                .split_once(':')
                .map(|(namespace, _)| namespace.to_string())
            {
                Some(namespace) => grouped
                    .entry(namespace)
                    .or_default()
                    .push(CapabilityFlagCount { flag, clients }),
                None => free_form.push(CapabilityFlagCount { flag, clients }),
            }
            (grouped, free_form)
        },
    );

    Ok(CapabilityVocabulary {
        namespaces: grouped
            .into_iter()
            .map(|(namespace, flags)| CapabilityNamespace {
                // A group's flags share a namespace, so its first flag decides the source.
                source: match flags.first() {
                    Some(f) if is_reserved_capability(&f.flag) => CapabilitySource::Derived,
                    Some(f) if is_tag_capability(&f.flag) => CapabilitySource::Assigned,
                    _ => CapabilitySource::Declared,
                },
                namespace,
                flags,
            })
            .collect(),
        free_form,
        clients: selected.len(),
        unreadable: fleet.unreadable.len(),
    })
}

#[cfg(test)]
mod tests {
    use rstest::rstest;
    use serde_json::json;

    use pipette_core::client::ClientStatus;
    use pipette_core::config::{Config, StorageConfig};
    use pipette_plan_types::{ContactEmail, NonEmptyTrimmedString, PublicKeyHex};
    use pipette_store::Stores;
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn stores_in(dir: &std::path::Path) -> anyhow::Result<Stores> {
        let config = Config {
            storage: StorageConfig::local_fs(dir.to_path_buf()),
            auth_storage: StorageConfig::local_fs(dir.to_path_buf()),
            ..Config::default()
        };
        build_local_fs_stores(&config)
    }

    /// Use a distinct `pk_suffix` for each client: the public key identifies it too.
    fn client(id: &str, pk_suffix: &str, flags: &[&str]) -> anyhow::Result<Client> {
        Ok(Client {
            client_id: ClientId::try_new(id)?,
            public_key: PublicKeyHex::try_new(format!(
                "abcdef0123456789abcdef0123456789abcdef0123456789abcdef01234567{pk_suffix}"
            ))?,
            organization: NonEmptyTrimmedString::try_new("org")?,
            client_details: NonEmptyTrimmedString::try_new("details")?,
            contact_email: ContactEmail::try_new("a@b.com")?,
            status: ClientStatus::Approved,
            registered_at: chrono::Utc::now(),
            device_profile: Default::default(),
            capabilities: flags.iter().map(|flag| flag.to_string()).collect(),
        })
    }

    /// The roster includes a client with `Pending` status because the filter
    /// tests every registered client. The scheduler checks approval.
    #[rstest]
    #[case::no_filter(&[], &[], &[], &[], &[], &["c-android", "c-ios", "c-iphone"])]
    #[case::requires_narrows(&[], &["os:ios"], &[], &[], &[], &["c-ios"])]
    #[case::requires_and_any_of(&[], &["os:ios"], &[&["gpu:a17pro", "gpu:a18pro"][..]], &[], &[], &["c-ios"])]
    #[case::any_of_alone_matches_nobody(&[], &[], &[&["gpu:a17pro"][..]], &[], &[], &[])]
    #[case::comma_flag_matches_whole(&[], &["device:iphone14,2"], &[], &[], &[], &["c-iphone"])]
    #[case::excludes_removes_a_match(&[], &["os:ios"], &[], &["gpu:a17pro"], &[], &[])]
    #[case::excludes_spares_the_rest(&[], &["os:ios"], &[], &["gpu:a18pro"], &[], &["c-ios"])]
    #[case::not_all_of_whole_group(&[], &["os:ios"], &[], &[], &[&["os:ios", "gpu:a17pro"][..]], &[])]
    #[case::not_all_of_partial_group(&[], &["os:ios"], &[], &[], &[&["os:ios", "gpu:a18pro"][..]], &["c-ios"])]
    #[case::excludes_alone_matches_nobody(&[], &[], &[], &["gpu:a17pro"], &[], &[])]
    #[case::not_all_of_alone_matches_nobody(&[], &[], &[], &[], &[&["os:ios"][..]], &[])]
    #[case::one_tag(&["us-east"], &[], &[], &[], &[], &["c-android", "c-ios"])]
    #[case::two_tags_intersect(&["us-east", "team-mobile"], &[], &[], &[], &[], &["c-ios"])]
    #[case::tag_and_capability(&["us-east"], &["os:android"], &[], &[], &[], &["c-android"])]
    // A capability clause reads a client's tags as `tag:` flags.
    #[case::requires_a_tag_flag(&[], &["tag:us-east"], &[], &[], &[], &["c-android", "c-ios"])]
    #[case::excludes_a_tag_flag(&[], &["tag:us-east"], &[], &["tag:team-mobile"], &[], &["c-android"])]
    #[tokio::test]
    async fn select_clients_applies_the_filters(
        #[case] tags: &[&str],
        #[case] requires: &[&str],
        #[case] any_of: &[&[&str]],
        #[case] excludes: &[&str],
        #[case] not_all_of: &[&[&str]],
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let clients = stores.clients.as_ref();
        clients
            .put_client(&client("c-ios", "01", &["os:ios", "gpu:a17pro"])?)
            .await?;
        clients
            .put_client(&client("c-android", "02", &["os:android"])?)
            .await?;
        let mut pending = client("c-iphone", "03", &["device:iphone14,2"])?;
        pending.status = ClientStatus::Pending;
        clients.put_client(&pending).await?;
        for (id, tag) in [
            ("c-ios", "us-east"),
            ("c-ios", "team-mobile"),
            ("c-android", "us-east"),
        ] {
            stores
                .tags
                .add_client_tag(&ClientId::try_new(id)?, &Tag::try_new(tag)?)
                .await?;
        }

        let flags = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
        let groups = |list: &[&[&str]]| list.iter().map(|group| flags(group)).collect();
        let query = CapabilityQuery {
            requires: flags(requires),
            any_of: groups(any_of),
            excludes: flags(excludes),
            not_all_of: groups(not_all_of),
        };
        let tags: Vec<Tag> = tags
            .iter()
            .map(|t| Tag::try_new(*t))
            .collect::<Result<_, _>>()?;

        let selected = select_clients(clients, stores.tags.as_ref(), &tags, &query).await?;

        let mut ids: Vec<String> = selected.iter().map(|c| c.client_id.to_string()).collect();
        ids.sort();
        assert_eq!(ids, expected);
        Ok(())
    }

    #[rstest]
    #[case::empty(&[], &[], &[], &[], false, false)]
    #[case::requires_alone(&["os:ios"], &[], &[], &[], true, false)]
    #[case::requires_and_a_negative(&["os:ios"], &[], &["gpu:a17pro"], &[], true, false)]
    #[case::any_of_alone(&[], &[&["gpu:a17pro"][..]], &[], &[], true, true)]
    #[case::excludes_alone(&[], &[], &["gpu:a17pro"], &[], true, true)]
    #[case::not_all_of_alone(&[], &[], &[], &[&["os:ios"][..]], true, true)]
    fn matches_nobody_needs_requires(
        #[case] requires: &[&str],
        #[case] any_of: &[&[&str]],
        #[case] excludes: &[&str],
        #[case] not_all_of: &[&[&str]],
        #[case] present: bool,
        #[case] nobody: bool,
    ) {
        let flags = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
        let groups = |list: &[&[&str]]| list.iter().map(|group| flags(group)).collect();
        let query = CapabilityQuery {
            requires: flags(requires),
            any_of: groups(any_of),
            excludes: flags(excludes),
            not_all_of: groups(not_all_of),
        };
        assert_eq!(query.is_present(), present);
        assert_eq!(query.matches_nobody(), nobody);
    }

    /// Two clients report `runtime:llama_cpp`, so its count checks the aggregate.
    #[rstest]
    #[case::every_status(
        None,
        json!({
            "namespaces": [
                {
                    "namespace": "os",
                    "source": "derived",
                    "flags": [{"flag": "os:ios", "clients": 1}]
                },
                {
                    "namespace": "runtime",
                    "source": "declared",
                    "flags": [
                        {"flag": "runtime:llama_cpp", "clients": 2},
                        {"flag": "runtime:onnx", "clients": 1}
                    ]
                }
            ],
            "free_form": [{"flag": "experimental", "clients": 1}],
            "clients": 3,
            "unreadable": 0
        })
    )]
    #[case::approved(
        Some(ClientStatus::Approved),
        json!({
            "namespaces": [
                {
                    "namespace": "os",
                    "source": "derived",
                    "flags": [{"flag": "os:ios", "clients": 1}]
                },
                {
                    "namespace": "runtime",
                    "source": "declared",
                    "flags": [{"flag": "runtime:llama_cpp", "clients": 2}]
                }
            ],
            "free_form": [{"flag": "experimental", "clients": 1}],
            "clients": 2,
            "unreadable": 0
        })
    )]
    #[case::pending(
        Some(ClientStatus::Pending),
        json!({
            "namespaces": [
                {
                    "namespace": "runtime",
                    "source": "declared",
                    "flags": [{"flag": "runtime:onnx", "clients": 1}]
                }
            ],
            "free_form": [],
            "clients": 1,
            "unreadable": 0
        })
    )]
    #[tokio::test]
    async fn capability_vocabulary_groups_counts_and_filters(
        #[case] status: Option<ClientStatus>,
        #[case] expected: Value,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let clients = stores.clients.as_ref();

        // The device profile supplies `os:ios`. Registration rejects
        // client-reported flags in reserved namespaces.
        let mut ios = client("c-ios", "01", &["runtime:llama_cpp", "experimental"])?;
        ios.device_profile.device_os_name = Some(NonEmptyTrimmedString::try_new("iOS")?);
        clients.put_client(&ios).await?;
        clients
            .put_client(&client("c-android", "02", &["runtime:llama_cpp"])?)
            .await?;
        let mut pending = client("c-onnx", "03", &["runtime:onnx"])?;
        pending.status = ClientStatus::Pending;
        clients.put_client(&pending).await?;

        let vocabulary = capability_vocabulary(clients, stores.tags.as_ref(), status).await?;

        assert_eq!(serde_json::to_value(&vocabulary)?, expected);
        Ok(())
    }

    #[rstest]
    #[case::every_status(None, 1)]
    #[case::approved(Some(ClientStatus::Approved), 1)]
    #[case::pending(Some(ClientStatus::Pending), 0)]
    #[tokio::test]
    async fn capability_vocabulary_reports_unreadable_records(
        #[case] status: Option<ClientStatus>,
        #[case] expected_clients: usize,
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let clients = stores.clients.as_ref();
        clients
            .put_client(&client("c-readable", "01", &["runtime:llama_cpp"])?)
            .await?;
        clients
            .put_client(&client("c-unreadable", "02", &["runtime:onnx"])?)
            .await?;
        std::fs::write(dir.path().join("clients/c-unreadable.json"), b"{")?;

        let vocabulary = capability_vocabulary(clients, stores.tags.as_ref(), status).await?;
        let namespaces = if expected_clients == 0 {
            json!([])
        } else {
            json!([{
                "namespace": "runtime",
                "source": "declared",
                "flags": [{"flag": "runtime:llama_cpp", "clients": 1}]
            }])
        };
        assert_eq!(
            serde_json::to_value(&vocabulary)?,
            json!({
                "namespaces": namespaces,
                "free_form": [],
                "clients": expected_clients,
                "unreadable": 1
            })
        );
        Ok(())
    }

    /// A tag counts as the `tag:` flag a job requires, and a `tag:` flag a
    /// client reported itself counts for nothing, as in the matcher.
    #[tokio::test]
    async fn capability_vocabulary_counts_assigned_tags() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;
        let clients = stores.clients.as_ref();
        clients
            .put_client(&client("c-tagged", "01", &["runtime:llama_cpp"])?)
            .await?;
        clients
            .put_client(&client("c-self-tagged", "02", &["tag:lab"])?)
            .await?;
        stores
            .tags
            .add_client_tag(&ClientId::try_new("c-tagged")?, &Tag::try_new("lab")?)
            .await?;

        let vocabulary = capability_vocabulary(clients, stores.tags.as_ref(), None).await?;

        assert_eq!(
            serde_json::to_value(&vocabulary)?,
            json!({
                "namespaces": [
                    {
                        "namespace": "runtime",
                        "source": "declared",
                        "flags": [{"flag": "runtime:llama_cpp", "clients": 1}]
                    },
                    {
                        "namespace": "tag",
                        "source": "assigned",
                        "flags": [{"flag": "tag:lab", "clients": 1}]
                    }
                ],
                "free_form": [],
                "clients": 2,
                "unreadable": 0
            })
        );
        Ok(())
    }

    #[tokio::test]
    async fn capability_vocabulary_of_an_empty_fleet_is_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let stores = stores_in(dir.path())?;

        let vocabulary =
            capability_vocabulary(stores.clients.as_ref(), stores.tags.as_ref(), None).await?;

        assert_eq!(
            serde_json::to_value(&vocabulary)?,
            json!({
                "namespaces": [],
                "free_form": [],
                "clients": 0,
                "unreadable": 0
            })
        );
        Ok(())
    }
}
