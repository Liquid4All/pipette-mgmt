use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use anyhow::Context;
use async_trait::async_trait;
use chrono::TimeZone;
use httpmock::{Method::GET, MockServer};
use rstest::rstest;
use serde_json::{Value, json};

use pipette_store::{DescriptorEntryPage, DigestHit, SetStateOutcome, TransitionOutcome};

use super::*;

#[derive(Default)]
struct Memory(
    Mutex<BTreeMap<String, DescriptorEntry>>,
    Arc<tokio::sync::Mutex<()>>,
    Mutex<Vec<(String, DescriptorEntryState)>>,
);

struct MemoryImportLock {
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

#[async_trait]
impl pipette_store::CatalogEntryLock for MemoryImportLock {
    async fn release(self: Box<Self>) -> anyhow::Result<()> {
        Ok(())
    }
}

impl Memory {
    fn transition_state_inner(
        &self,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?;
        let Some(entry) = entries.get_mut(&name.key()) else {
            return Ok(TransitionOutcome::NoSuchEntry);
        };
        if entry.state != expected {
            return Ok(TransitionOutcome::UnexpectedState(entry.state));
        }
        entry.state = state;
        Ok(TransitionOutcome::Changed)
    }

    fn entry(&self, name: &str) -> anyhow::Result<DescriptorEntry> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?
            .get(name)
            .cloned()
            .with_context(|| format!("no entry {name}"))
    }

    fn created_state(&self, name: &str) -> anyhow::Result<DescriptorEntryState> {
        self.2
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog creation log lock failed"))?
            .iter()
            .rev()
            .find(|(created, _)| created == name)
            .map(|(_, state)| *state)
            .with_context(|| format!("no creation record for {name}"))
    }

    fn is_empty(&self) -> anyhow::Result<bool> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?
            .is_empty())
    }
}

#[async_trait]
impl DescriptorCatalogStore for Memory {
    async fn lock_importer_entries(
        &self,
    ) -> anyhow::Result<Box<dyn pipette_store::CatalogEntryLock>> {
        Ok(Box::new(MemoryImportLock {
            _guard: self.1.clone().lock_owned().await,
        }))
    }

    async fn create_entry(&self, entry: &DescriptorEntry) -> anyhow::Result<CreateEntryOutcome> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?;
        if let Some(old) = entries.get(&entry.name.key()) {
            return Ok(CreateEntryOutcome::NameTaken {
                name: old.name.clone(),
                state: old.state,
            });
        }
        entries.insert(entry.name.key(), entry.clone());
        self.2
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog creation log lock failed"))?
            .push((entry.name.key(), entry.state));
        Ok(CreateEntryOutcome::Created)
    }

    async fn append_members(
        &self,
        _scope: &pipette_plan_types::CatalogScope,
        name: &CatalogName,
        members: NonEmptyVec<DescriptorMember>,
    ) -> anyhow::Result<AppendOutcome> {
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?;
        let Some(entry) = entries.get_mut(&name.key()) else {
            return Ok(AppendOutcome::NoSuchEntry);
        };
        let mut all = entry.members.iter().cloned().collect::<Vec<_>>();
        all.extend(members.into_inner());
        entry.members = NonEmptyVec::try_new(all)?;
        Ok(AppendOutcome::Appended(entry.clone()))
    }

    async fn set_state(
        &self,
        _scope: &pipette_plan_types::CatalogScope,
        name: &CatalogName,
        state: DescriptorEntryState,
    ) -> anyhow::Result<SetStateOutcome> {
        let _importer_lock = self.1.lock().await;
        let mut entries = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?;
        let Some(entry) = entries.get_mut(&name.key()) else {
            return Ok(SetStateOutcome::NoSuchEntry);
        };
        let from = entry.state;
        entry.state = state;
        Ok(SetStateOutcome::Changed { from })
    }

    async fn transition_state(
        &self,
        _scope: &pipette_plan_types::CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome> {
        let _importer_lock = self.1.lock().await;
        self.transition_state_inner(name, expected, state)
    }

    async fn transition_state_under_importer_lock(
        &self,
        _scope: &pipette_plan_types::CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome> {
        self.transition_state_inner(name, expected, state)
    }

    async fn get_entry(
        &self,
        _scope: &pipette_plan_types::CatalogScope,
        name: &CatalogName,
    ) -> anyhow::Result<Option<DescriptorEntry>> {
        Ok(self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?
            .get(&name.key())
            .cloned())
    }

    async fn list_entries(
        &self,
        filter: &DescriptorEntryFilter,
        after: Option<&pipette_store::CatalogEntryKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<DescriptorEntryPage> {
        let mut items = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("catalog store lock failed"))?
            .values()
            .filter(|entry| filter.kinds.is_empty() || filter.kinds.contains(&entry.kind))
            .filter(|entry| filter.states.is_empty() || filter.states.contains(&entry.state))
            .filter(|entry| {
                filter
                    .name_prefix
                    .as_ref()
                    .is_none_or(|prefix| entry.name.key().starts_with(&prefix.to_lowercase()))
            })
            .cloned()
            .collect::<Vec<_>>();
        items.sort_by(|a, b| {
            b.release_time
                .cmp(&a.release_time)
                .then_with(|| a.name.key().cmp(&b.name.key()))
        });
        if let Some(after) = after
            && let Some(position) = items
                .iter()
                .position(|entry| entry.name.key() == after.name.key())
        {
            items.drain(..=position);
        }
        let resume_after = if items.len() > limit.get() {
            items
                .get(limit.get() - 1)
                .map(|entry| pipette_store::CatalogEntryKey {
                    scope: entry.scope.clone(),
                    name: entry.name.clone(),
                })
        } else {
            None
        };
        items.truncate(limit.get());
        Ok(DescriptorEntryPage {
            items,
            unreadable: Vec::new(),
            resume_after,
        })
    }

    async fn find_by_digests(
        &self,
        _digests: &[String],
    ) -> anyhow::Result<HashMap<String, Vec<DigestHit>>> {
        Ok(HashMap::new())
    }

    async fn live_entries_in_org(
        &self,
        _org_id: &pipette_plan_types::OrgId,
    ) -> anyhow::Result<Vec<CatalogName>> {
        Ok(Vec::new())
    }
    async fn delete_tombstones_in_org(
        &self,
        _org_id: &pipette_plan_types::OrgId,
    ) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Runs(Mutex<Vec<CatalogImportRunFinish>>);

impl Runs {
    fn last(&self) -> anyhow::Result<CatalogImportRunFinish> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("run store lock failed"))?
            .last()
            .cloned()
            .context("no finished run")
    }
}

#[async_trait]
impl CatalogImportRunStore for Runs {
    async fn recent_runs(
        &self,
        _limit: std::num::NonZeroUsize,
    ) -> anyhow::Result<pipette_plan_types::CatalogImportRuns> {
        Ok(pipette_plan_types::CatalogImportRuns {
            items: Vec::new(),
            last_successful_import_at: None,
        })
    }

    async fn start_run(
        &self,
        _id: &str,
        _command: &str,
        _target: Option<&CatalogName>,
        _at: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    async fn finish_run(
        &self,
        _id: &str,
        _at: DateTime<Utc>,
        finish: &CatalogImportRunFinish,
    ) -> anyhow::Result<()> {
        self.0
            .lock()
            .map_err(|_| anyhow::anyhow!("run store lock failed"))?
            .push(finish.clone());
        Ok(())
    }
}

fn fixture(name: &str) -> anyhow::Result<Value> {
    let body = match name {
        "stable" => include_str!("../fixtures/v0.7.0.json"),
        "last" => include_str!("../fixtures/b11669.json"),
        "earlier" => include_str!("../fixtures/b11662.json"),
        "last-assets" => include_str!("../fixtures/b11669-assets.json"),
        "earlier-assets" => include_str!("../fixtures/b11662-assets.json"),
        _ => anyhow::bail!("no fixture {name}"),
    };
    Ok(serde_json::from_str(body)?)
}

/// Faults that a test adds to the recorded upstream.
#[derive(Default)]
struct Faults {
    /// b11669 has an asset that no flavor names.
    unknown: bool,
    /// The assets of the later b11670 build return 401.
    fail_later: bool,
    /// v0.7.0 has no uploaded `nightly-tag.txt`.
    no_nightly_tag: bool,
    /// The `b<N>` builds are full releases, as before 2026-08-21.
    full_releases: bool,
}

async fn upstream(faults: Faults) -> anyhow::Result<MockServer> {
    let server = MockServer::start_async().await;
    let mut releases = vec![fixture("stable")?, fixture("last")?, fixture("earlier")?];
    if faults.full_releases {
        for build in &mut releases[1..] {
            build
                .as_object_mut()
                .context("release fixture is not an object")?
                .insert("prerelease".into(), json!(false));
        }
    }
    releases.push(json!({"id": 70, "tag_name": "b11670", "published_at": "2026-10-15T20:00:00Z", "prerelease": true, "draft": false}));
    releases.push(json!({"id": 71, "tag_name": "b11671", "published_at": "2026-10-15T23:59:00Z", "prerelease": true, "draft": false}));
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases")
                .query_param("page", "1")
                .header("authorization", "Bearer test-token");
            then.status(200).json_body(json!(releases));
        })
        .await;
    let stable_id = fixture("stable")?
        .get("id")
        .and_then(Value::as_i64)
        .context("stable id is missing")?;
    server
        .mock_async(move |when, then| {
            when.method(GET).path(format!(
                "/repos/ggml-org/llama.cpp/releases/{stable_id}/assets"
            ));
            if faults.no_nightly_tag {
                then.status(200).json_body(json!([]));
            } else {
                then.status(200).json_body(
                    json!([{"id": 644579765, "name": "nightly-tag.txt", "state": "uploaded"}]),
                );
            }
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/assets/644579765");
            then.status(200).body("b11669\n");
        })
        .await;
    let last = fixture("last")?;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/tags/b11669");
            then.status(200).json_body(last);
        })
        .await;
    let earlier_assets = fixture("earlier-assets")?;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/413984595/assets");
            then.status(200).json_body(earlier_assets);
        })
        .await;
    let mut last_assets = fixture("last-assets")?;
    if faults.unknown {
        last_assets.as_array_mut().context("asset fixture is not an array")?.push(
            json!({"id": 99, "name": "llama-b11669-bin-new-flavor.tar.gz", "state": "uploaded"}),
        );
    }
    server
        .mock_async(move |when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/414013709/assets");
            then.status(200).json_body(last_assets);
        })
        .await;
    server.mock_async(move |when, then| {
        when.method(GET).path("/repos/ggml-org/llama.cpp/releases/70/assets");
        if faults.fail_later {
            then.status(401);
        } else {
            then.status(200).json_body(json!([{"id": 700, "name": "llama-b11670-bin-macos-arm64.tar.gz", "state": "uploaded"}]));
        }
    }).await;
    // b11671 is younger than the minimum age, except for a test with a late
    // `now`. It has no uploaded assets.
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/71/assets");
            then.status(200).json_body(json!([]));
        })
        .await;
    Ok(server)
}

fn now() -> anyhow::Result<DateTime<Utc>> {
    Utc.with_ymd_and_hms(2026, 10, 16, 0, 30, 0)
        .single()
        .context("test time is invalid")
}

fn api(server: &MockServer) -> anyhow::Result<GitHubClient> {
    GitHubClient::with_base(
        GitHubToken::new("test-token".into()),
        Url::parse(&format!("{}/", server.base_url()))?,
    )
}

#[tokio::test]
async fn a_stable_and_the_last_complete_daily_share_one_build() -> anyhow::Result<()> {
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let runs = Runs::default();
    let mut config = DescriptorCatalogConfig::default();
    config.precedence.llamacpp = vec!["macos-x64".into(), "macos-arm64".into()];
    let first = import(&api(&server)?, &store, &runs, &config, None, now()?).await?;
    assert_eq!((first.entries_created, first.members_added), (2, 56));
    let stable = store.entry("llamacpp-v0.7.0")?;
    let daily = store.entry("llamacpp-daily-2026-10-15")?;
    assert_eq!(entry_tag(&stable)?, "b11669");
    assert_eq!(entry_tag(&daily)?, "b11669");
    assert!(
        serde_json::to_value(daily.members.head().runtime())?
            .get("asset")
            .is_none()
    );
    assert_eq!(
        stable.release_time,
        fixture("stable")?
            .get("published_at")
            .and_then(Value::as_str)
            .context("stable publication time is missing")?
            .parse::<DateTime<Utc>>()?
    );
    assert_eq!(daily.members.head().flavor(), Some("macos-x64"));
    assert_eq!(
        daily.members.get(1).and_then(DescriptorMember::flavor),
        Some("macos-arm64")
    );
    let second = import(&api(&server)?, &store, &runs, &config, None, now()?).await?;
    assert_eq!((second.entries_created, second.members_added), (0, 0));
    Ok(())
}

#[tokio::test]
async fn since_backfills_a_finished_utc_day() -> anyhow::Result<()> {
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let runs = Runs::default();
    let config = DescriptorCatalogConfig::default();
    let after = NaiveDate::from_ymd_opt(2026, 10, 16).context("invalid date")?;
    let first = import(&api(&server)?, &store, &runs, &config, Some(after), now()?).await?;
    assert_eq!(first.entries_created, 1);
    assert!(
        store
            .get_entry(
                &pipette_plan_types::CatalogScope::Global,
                &CatalogName::try_new("llamacpp-daily-2026-10-15")?
            )
            .await?
            .is_none()
    );
    let day = NaiveDate::from_ymd_opt(2026, 10, 15).context("invalid date")?;
    let second = import(&api(&server)?, &store, &runs, &config, Some(day), now()?).await?;
    assert_eq!(second.entries_created, 1);
    assert_eq!(
        entry_tag(&store.entry("llamacpp-daily-2026-10-15")?)?,
        "b11669"
    );
    Ok(())
}

#[tokio::test]
async fn default_since_uses_the_newest_llamacpp_daily_entry() -> anyhow::Result<()> {
    let store = Memory::default();
    let at = now()?;
    for (name, time) in [
        ("llamacpp-daily-2026-10-15", at - chrono::Duration::days(10)),
        ("other-daily-2026-10-20", at),
    ] {
        let entry = DescriptorEntry::new(
            pipette_plan_types::CatalogScope::Global,
            CatalogName::try_new(name)?,
            DescriptorEntryKind::Daily,
            DescriptorEntryState::Active,
            time,
            time,
            DescriptorEntryCreator::ImporterLlamacpp,
            NonEmptyVec::try_new(vec![descriptor(
                "b11669",
                &LlamaCppFlavor::known().next().context("no flavor")?,
                time,
            )?])?,
        )?;
        store.create_entry(&entry).await?;
    }
    assert_eq!(
        default_since(&store, at.date_naive()).await?,
        NaiveDate::from_ymd_opt(2026, 10, 13).context("invalid date")?
    );
    Ok(())
}

#[rstest]
#[case::unknown_asset(
    Faults { unknown: true, ..Faults::default() },
    (2, 56),
    &[("llamacpp-v0.7.0", 28), ("llamacpp-daily-2026-10-15", 28)],
    &[],
    &["b11669/llama-b11669-bin-new-flavor.tar.gz"],
    "1 unclassified assets",
)]
#[case::later_build_fails(
    Faults { fail_later: true, ..Faults::default() },
    (1, 28),
    &[("llamacpp-v0.7.0", 28)],
    &["llamacpp-daily-2026-10-15"],
    &[],
    "401",
)]
#[case::no_nightly_tag(
    Faults { no_nightly_tag: true, ..Faults::default() },
    (1, 28),
    &[("llamacpp-daily-2026-10-15", 28)],
    &["llamacpp-v0.7.0"],
    &[],
    "v0.7.0: v0.7.0 has no uploaded nightly-tag.txt",
)]
#[tokio::test]
async fn a_failed_import_keeps_what_it_created(
    #[case] faults: Faults,
    #[case] counts: (i32, i32),
    #[case] present: &[(&str, usize)],
    #[case] absent: &[&str],
    #[case] unclassified: &[&str],
    #[case] error: &str,
) -> anyhow::Result<()> {
    let server = upstream(faults).await?;
    let store = Memory::default();
    let runs = Runs::default();
    assert!(
        import(
            &api(&server)?,
            &store,
            &runs,
            &DescriptorCatalogConfig::default(),
            None,
            now()?
        )
        .await
        .is_err()
    );
    for (name, members) in present {
        assert_eq!(store.entry(name)?.members.len(), *members, "{name}");
    }
    for name in absent {
        assert!(
            store
                .get_entry(
                    &pipette_plan_types::CatalogScope::Global,
                    &CatalogName::try_new(*name)?
                )
                .await?
                .is_none(),
            "{name}"
        );
    }
    let finish = runs.last()?;
    assert_eq!((finish.entries_created, finish.members_added), counts);
    assert_eq!(finish.unclassified_assets, unclassified);
    assert!(
        finish
            .error_summary
            .as_deref()
            .context("no run error")?
            .contains(error)
    );
    Ok(())
}

/// A GitHub server whose release list answers with one status and headers.
async fn releases_only(status: u16, headers: &[(&str, &str)]) -> MockServer {
    let server = MockServer::start_async().await;
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
        .collect();
    server
        .mock_async(move |when, then| {
            when.method(GET).path("/repos/ggml-org/llama.cpp/releases");
            headers
                .into_iter()
                .fold(then.status(status), |then, (name, value)| {
                    then.header(name, value)
                });
        })
        .await;
    server
}

#[rstest]
#[case::unauthorized(401, &[], "401")]
#[case::rate_limited(
    403,
    &[("X-RateLimit-Remaining", "0"), ("X-RateLimit-Reset", "1792108800")],
    "rate limit reached until 2026-10-16T00:00:00+00:00",
)]
#[tokio::test]
async fn a_release_list_error_finishes_the_run_as_error(
    #[case] status: u16,
    #[case] headers: &[(&str, &str)],
    #[case] error: &str,
) -> anyhow::Result<()> {
    let server = releases_only(status, headers).await;
    let runs = Runs::default();
    assert!(
        import(
            &api(&server)?,
            &Memory::default(),
            &runs,
            &DescriptorCatalogConfig::default(),
            None,
            now()?
        )
        .await
        .is_err()
    );
    assert!(
        runs.last()?
            .error_summary
            .as_deref()
            .context("no run error")?
            .contains(error)
    );
    Ok(())
}
#[tokio::test]
async fn a_full_release_build_gets_a_daily_entry() -> anyhow::Result<()> {
    let server = upstream(Faults {
        full_releases: true,
        ..Faults::default()
    })
    .await?;
    let store = Memory::default();
    import(
        &api(&server)?,
        &store,
        &Runs::default(),
        &DescriptorCatalogConfig::default(),
        None,
        now()?,
    )
    .await?;
    assert_eq!(
        entry_tag(&store.entry("llamacpp-daily-2026-10-15")?)?,
        "b11669"
    );
    Ok(())
}

#[tokio::test]
async fn refresh_appends_to_a_hidden_entry() -> anyhow::Result<()> {
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let runs = Runs::default();
    let name = CatalogName::try_new("llamacpp-v0.7.0")?;
    let members = NonEmptyVec::try_new(vec![descriptor(
        "b11669",
        &LlamaCppFlavor::MacosArm64,
        now()?,
    )?])?;
    let entry = DescriptorEntry::new(
        pipette_plan_types::CatalogScope::Global,
        name.clone(),
        DescriptorEntryKind::Stable,
        DescriptorEntryState::Hidden,
        now()?,
        now()?,
        DescriptorEntryCreator::ImporterLlamacpp,
        members,
    )?;
    store.create_entry(&entry).await?;
    let result = refresh(
        &api(&server)?,
        &store,
        &runs,
        &DescriptorCatalogConfig::default(),
        &name,
        now()?,
    )
    .await?;
    assert_eq!(result.members_added, 27);
    let changed = store.entry("llamacpp-v0.7.0")?;
    assert_eq!(changed.state, DescriptorEntryState::Hidden);
    assert_eq!(changed.members.head().flavor(), Some("macos-arm64"));
    assert_eq!(
        changed.members.get(1).and_then(DescriptorMember::flavor),
        Some("macos-x64")
    );
    assert_eq!(changed.members.len(), 28);
    Ok(())
}

#[tokio::test]
async fn import_appends_a_new_flavor_to_an_active_entry() -> anyhow::Result<()> {
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let runs = Runs::default();
    let entry = DescriptorEntry::new(
        pipette_plan_types::CatalogScope::Global,
        CatalogName::try_new("llamacpp-v0.7.0")?,
        DescriptorEntryKind::Stable,
        DescriptorEntryState::Active,
        now()?,
        now()?,
        DescriptorEntryCreator::ImporterLlamacpp,
        NonEmptyVec::try_new(vec![descriptor(
            "b11669",
            &LlamaCppFlavor::MacosArm64,
            now()?,
        )?])?,
    )?;
    store.create_entry(&entry).await?;
    let result = import(
        &api(&server)?,
        &store,
        &runs,
        &DescriptorCatalogConfig::default(),
        None,
        now()?,
    )
    .await?;
    assert_eq!((result.entries_created, result.members_added), (1, 55));
    let changed = store.entry("llamacpp-v0.7.0")?;
    assert_eq!(changed.members.len(), 28);
    assert_eq!(changed.members.head().flavor(), Some("macos-arm64"));
    Ok(())
}

/// Put an entry with all known flavors of b11669 in `store`, released
/// `age_days` before `at`.
async fn seed(
    store: &Memory,
    name: &str,
    kind: DescriptorEntryKind,
    state: DescriptorEntryState,
    age_days: i64,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let released = at - chrono::Duration::days(age_days);
    let creator = match kind {
        DescriptorEntryKind::HandMade => DescriptorEntryCreator::Operator,
        DescriptorEntryKind::Stable | DescriptorEntryKind::Daily => {
            DescriptorEntryCreator::ImporterLlamacpp
        }
    };
    let members = LlamaCppFlavor::known()
        .filter(|flavor| flavor.release_asset_name("b11669").is_some())
        .map(|flavor| descriptor("b11669", &flavor, released))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let entry = DescriptorEntry::new(
        pipette_plan_types::CatalogScope::Global,
        CatalogName::try_new(name)?,
        kind,
        state,
        released,
        released,
        creator,
        NonEmptyVec::try_new(members)?,
    )?;
    store.create_entry(&entry).await?;
    Ok(())
}

fn states(store: &Memory, names: &[&str]) -> anyhow::Result<Vec<DescriptorEntryState>> {
    names
        .iter()
        .map(|name| store.entry(name).map(|entry| entry.state))
        .collect()
}

#[tokio::test]
async fn importer_lock_serializes_operator_state_changes() -> anyhow::Result<()> {
    use std::time::Duration;

    let store = Arc::new(Memory::default());
    seed(
        &store,
        "llamacpp-v0.7.0",
        DescriptorEntryKind::Stable,
        DescriptorEntryState::Active,
        1,
        now()?,
    )
    .await?;
    let importer_lock = store.lock_importer_entries().await?;
    let operator_store = Arc::clone(&store);
    let name = CatalogName::try_new("llamacpp-v0.7.0")?;
    let mut operator = tokio::spawn(async move {
        operator_store
            .transition_state(
                &pipette_plan_types::CatalogScope::Global,
                &name,
                DescriptorEntryState::Active,
                DescriptorEntryState::Hidden,
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut operator)
            .await
            .is_err()
    );
    importer_lock.release().await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), operator).await???,
        TransitionOutcome::Changed
    );
    Ok(())
}

#[tokio::test]
async fn an_import_hides_expired_entries_and_keeps_the_others() -> anyhow::Result<()> {
    use DescriptorEntryKind::{Daily, HandMade, Stable};
    use DescriptorEntryState::{Active, Hidden, Pinned};
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let runs = Runs::default();
    let at = now()?;
    seed(&store, "llamacpp-daily-2026-09-01", Daily, Active, 45, at).await?;
    seed(&store, "llamacpp-v0.1.0", Stable, Pinned, 300, at).await?;
    seed(&store, "llamacpp-v0.2.0", Stable, Active, 62, at).await?;
    seed(&store, "llamacpp-v0.3.0", Stable, Active, 61, at).await?;
    seed(&store, "fork", HandMade, Active, 300, at).await?;
    let result = import(
        &api(&server)?,
        &store,
        &runs,
        &DescriptorCatalogConfig::default(),
        None,
        at,
    )
    .await?;
    assert_eq!(
        states(
            &store,
            &[
                "llamacpp-daily-2026-09-01",
                "llamacpp-v0.1.0",
                "llamacpp-v0.2.0",
                "llamacpp-v0.3.0",
                "fork",
                "llamacpp-v0.7.0",
                "llamacpp-daily-2026-10-15",
            ]
        )?,
        [Hidden, Pinned, Hidden, Active, Active, Active, Active]
    );
    assert_eq!((result.entries_created, result.entries_hidden), (2, 2));
    assert_eq!(runs.last()?.entries_hidden, 2);
    Ok(())
}

#[tokio::test]
async fn a_backfilled_old_entry_starts_hidden() -> anyhow::Result<()> {
    use DescriptorEntryKind::Stable;
    use DescriptorEntryState::{Active, Hidden};
    let server = upstream(Faults::default()).await?;
    let store = Memory::default();
    let at = Utc
        .with_ymd_and_hms(2027, 1, 1, 3, 0, 0)
        .single()
        .context("test time is invalid")?;
    seed(&store, "llamacpp-v0.8.0", Stable, Active, 20, at).await?;
    seed(&store, "llamacpp-v0.9.0", Stable, Active, 10, at).await?;
    let since = NaiveDate::from_ymd_opt(2026, 10, 15).context("invalid date")?;
    let result = import(
        &api(&server)?,
        &store,
        &Runs::default(),
        &DescriptorCatalogConfig::default(),
        Some(since),
        at,
    )
    .await?;
    assert_eq!(
        states(
            &store,
            &[
                "llamacpp-v0.7.0",
                "llamacpp-daily-2026-10-15",
                "llamacpp-v0.8.0",
                "llamacpp-v0.9.0",
            ]
        )?,
        [Hidden, Hidden, Active, Active]
    );
    assert_eq!((result.entries_created, result.entries_hidden), (2, 2));
    assert_eq!(store.created_state("llamacpp-v0.7.0")?, Hidden);
    assert_eq!(store.created_state("llamacpp-daily-2026-10-15")?, Hidden);
    Ok(())
}

#[tokio::test]
async fn expiry_runs_after_a_failed_release_list() -> anyhow::Result<()> {
    let server = releases_only(401, &[]).await;
    let store = Memory::default();
    let runs = Runs::default();
    seed(
        &store,
        "llamacpp-daily-2026-09-01",
        DescriptorEntryKind::Daily,
        DescriptorEntryState::Active,
        45,
        now()?,
    )
    .await?;
    assert!(
        import(
            &api(&server)?,
            &store,
            &runs,
            &DescriptorCatalogConfig::default(),
            None,
            now()?
        )
        .await
        .is_err()
    );
    assert_eq!(
        store.entry("llamacpp-daily-2026-09-01")?.state,
        DescriptorEntryState::Hidden
    );
    let finish = runs.last()?;
    assert_eq!(finish.entries_hidden, 1);
    assert!(finish.error_summary.is_some());
    Ok(())
}

#[tokio::test]
async fn default_since_without_a_daily_entry_is_fourteen_days_back() -> anyhow::Result<()> {
    let today = NaiveDate::from_ymd_opt(2026, 10, 16).context("invalid date")?;
    assert_eq!(
        default_since(&Memory::default(), today).await?,
        NaiveDate::from_ymd_opt(2026, 10, 2).context("invalid date")?
    );
    Ok(())
}

#[rstest]
#[case::build("b11669", Some(11669))]
#[case::signed_build("b+5", None)]
#[case::empty_build("b", None)]
#[case::stable("v0.7.0", None)]
fn build_number_reads_only_digits(#[case] tag: &str, #[case] expected: Option<u32>) {
    assert_eq!(build_number(tag), expected);
}

#[rstest]
#[case::stable("v0.7.0", true)]
#[case::signed("v+1.+2.+3", false)]
#[case::short("v0.7", false)]
#[case::empty_part("v0..7", false)]
#[case::build("b11669", false)]
fn stable_version_reads_only_digits(#[case] tag: &str, #[case] expected: bool) {
    assert_eq!(stable_version(tag), expected);
}

#[test]
fn an_incomplete_build_names_its_missing_assets() -> anyhow::Result<()> {
    let present = LlamaCppFlavor::MacosArm64
        .release_asset_name("b11670")
        .context("macos-arm64 has no asset name")?;
    let classified = classify(
        "b11670",
        &[Asset {
            id: 700,
            name: present.clone(),
            state: "uploaded".into(),
        }],
        &DescriptorCatalogConfig::default(),
    );
    assert!(!classified.complete);
    assert!(!classified.missing.is_empty());
    assert!(!classified.missing.contains(&present));
    Ok(())
}

#[tokio::test]
async fn a_day_without_a_complete_build_gets_no_name() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    server
        .mock_async(|when, then| {
            when.method(GET).path("/repos/ggml-org/llama.cpp/releases");
            then.status(200).json_body(json!([{
                "id": 70, "tag_name": "b11670", "published_at": "2026-10-15T20:00:00Z",
                "prerelease": true, "draft": false
            }]));
        })
        .await;
    server.mock_async(|when, then| {
        when.method(GET).path("/repos/ggml-org/llama.cpp/releases/70/assets");
        then.status(200).json_body(json!([{"id": 700, "name": "llama-b11670-bin-macos-arm64.tar.gz", "state": "uploaded"}]));
    }).await;
    let store = Memory::default();
    let finish = import(
        &api(&server)?,
        &store,
        &Runs::default(),
        &DescriptorCatalogConfig::default(),
        None,
        now()?,
    )
    .await?;
    assert_eq!(finish.entries_created, 0);
    assert!(store.is_empty()?);
    Ok(())
}

#[tokio::test]
async fn github_pages_releases_and_assets() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let release = fixture("last")?;
    let first_page = vec![release.clone(); 100];
    server
        .mock_async(move |when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases")
                .query_param("page", "1");
            then.status(200).json_body(json!(first_page));
        })
        .await;
    server
        .mock_async(move |when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases")
                .query_param("page", "2");
            then.status(200).json_body(json!([release]));
        })
        .await;
    let first_assets = vec![json!({"id": 1, "name": "a", "state": "uploaded"}); 100];
    server
        .mock_async(move |when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/414013709/assets")
                .query_param("page", "1");
            then.status(200).json_body(json!(first_assets));
        })
        .await;
    server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases/414013709/assets")
                .query_param("page", "2");
            then.status(200)
                .json_body(json!([{"id": 2, "name": "b", "state": "uploaded"}]));
        })
        .await;
    let github = api(&server)?;
    assert_eq!(github.releases(None).await?.len(), 101);
    assert_eq!(
        github
            .assets(&serde_json::from_value(fixture("last")?)?)
            .await?
            .len(),
        101
    );
    Ok(())
}

#[tokio::test]
async fn release_paging_stops_at_an_older_release() -> anyhow::Result<()> {
    let server = MockServer::start_async().await;
    let first_page = vec![fixture("last")?; 100];
    server
        .mock_async(move |when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases")
                .query_param("page", "1");
            then.status(200).json_body(json!(first_page));
        })
        .await;
    let second_page = server
        .mock_async(|when, then| {
            when.method(GET)
                .path("/repos/ggml-org/llama.cpp/releases")
                .query_param("page", "2");
            then.status(200).json_body(json!([]));
        })
        .await;
    let until = Utc
        .with_ymd_and_hms(2026, 10, 15, 18, 0, 0)
        .single()
        .context("test time is invalid")?;
    assert_eq!(api(&server)?.releases(Some(until)).await?.len(), 100);
    assert_eq!(second_page.calls_async().await, 0);
    Ok(())
}
