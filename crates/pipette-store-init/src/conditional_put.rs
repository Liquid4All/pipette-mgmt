//! Startup probe that an `s3` endpoint honors the conditional `PUT`s the mutate
//! lock and pre-auth key spending rely on. An endpoint that ignores the header
//! answers `200`, which `object_store` reports as a won race.

use object_store::path::Path as ObjPath;
use object_store::{
    ObjectStore, ObjectStoreExt, PutMode, PutOptions, PutPayload, PutResult, UpdateVersion,
};

use pipette_core::config::StorageConfig;
use pipette_store_s3::{build_s3_object_store, obj_path};

/// Never read back; it names a key left behind by a process that died mid-probe.
const PROBE_BODY: &[u8] = b"pipette-mgmt conditional-put startup probe";

/// Must differ from [`PROBE_BODY`]: S3's ETag is content-derived, so the same
/// bytes would keep the stale version current and fail a conformant bucket.
const PROBE_BODY_REWRITE: &[u8] = b"pipette-mgmt conditional-put startup probe (rewrite)";

/// The conditional writes a config section's callers depend on.
/// `[auth_storage]` needs only `Create`: its swap is a best-effort record repair.
#[derive(Debug, Clone, Copy, strum::IntoStaticStr)]
pub enum Requires {
    /// Create-if-absent (`If-None-Match: *`) alone.
    #[strum(serialize = "create-if-absent")]
    Create,
    /// Also a compare-and-swap (`If-Match`), which the lock's lease takeover uses.
    #[strum(serialize = "create-if-absent, compare-and-swap")]
    CreateAndSwap,
}

impl Requires {
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// Fail unless the `s3` backend behind `storage` refuses conflicting writes for
/// `requires` (at one moment, not under concurrency); `local_fs` has `O_EXCL`.
/// `consequence` names only this section's mechanism: others may be conformant.
pub async fn validate(
    enabled: bool,
    storage: &StorageConfig,
    section: &str,
    consequence: &str,
    requires: Requires,
) -> anyhow::Result<()> {
    let StorageConfig::S3 { bucket, .. } = storage else {
        return Ok(());
    };
    if !enabled {
        tracing::info!(
            section,
            bucket,
            "conditional PUT probe skipped: probe_backend_capabilities is off"
        );
        return Ok(());
    }
    let (store, prefix) = build_s3_object_store(storage)?;
    validate_store(
        &*store,
        &probe_path(&prefix),
        section,
        bucket,
        consequence,
        requires,
    )
    .await
}

async fn validate_store(
    store: &dyn ObjectStore,
    path: &ObjPath,
    section: &str,
    bucket: &str,
    consequence: &str,
    requires: Requires,
) -> anyhow::Result<()> {
    let verdict = probe(store, path, section, bucket, consequence, requires).await;

    // Clean up whatever the verdict; a failed delete is not fatal, since the key
    // sits outside every prefix the stores list.
    match store.delete(path).await {
        Ok(()) | Err(object_store::Error::NotFound { .. }) => {}
        Err(e) => tracing::warn!(
            path = %path,
            error = %e,
            "failed to delete the conditional-put probe object"
        ),
    }

    verdict
}

async fn probe(
    store: &dyn ObjectStore,
    path: &ObjPath,
    section: &str,
    bucket: &str,
    consequence: &str,
    requires: Requires,
) -> anyhow::Result<()> {
    #[expect(
        clippy::wildcard_enum_match_arm,
        reason = "`object_store::Error` is `#[non_exhaustive]`; every other error passes through"
    )]
    let first = put(store, path, PROBE_BODY, PutMode::Create)
        .await
        .map_err(|e| match e {
            // `AWS_CONDITIONAL_PUT=disabled` reaches the builder via `from_env` and
            // turns every conditional write into this error.
            object_store::Error::NotImplemented { .. } => anyhow::anyhow!(
                "{section} bucket `{bucket}`: conditional PUT is unavailable ({e}), and \
                 {consequence}. Check `AWS_CONDITIONAL_PUT` in the environment (see \
                 docs/operations.md)"
            ),
            e => anyhow::Error::new(e),
        })?;

    match put(store, path, PROBE_BODY, PutMode::Create).await {
        Err(object_store::Error::AlreadyExists { .. }) => {}
        Ok(_) => {
            return Err(anyhow::anyhow!(
                "{section} bucket `{bucket}` does not honor conditional PUT: a second \
                 `If-None-Match: *` write to the same key succeeded instead of being refused, \
                 so {consequence}. Use a backend that implements conditional PUT (see \
                 docs/operations.md)"
            ));
        }
        // Auth, network, or bucket faults carry their own diagnosis.
        Err(e) => return Err(anyhow::Error::new(e)),
    }

    if matches!(requires, Requires::CreateAndSwap) {
        probe_swap(store, path, first, section, bucket).await?;
    }

    tracing::info!(
        section,
        bucket,
        writes = requires.as_str(),
        "conditional PUT validated"
    );
    Ok(())
}

/// Supersede `stale`, then require a compare-and-swap against it to be refused:
/// an endpoint that ignores `If-Match` hands an expired lease to both racers.
async fn probe_swap(
    store: &dyn ObjectStore,
    path: &ObjPath,
    stale: PutResult,
    section: &str,
    bucket: &str,
) -> anyhow::Result<()> {
    // The swap is an `If-Match` on the ETag, so without one there is no swap.
    let Some(e_tag) = stale.e_tag else {
        anyhow::bail!(
            "{section} bucket `{bucket}` returned no ETag for a create, so a compare-and-swap \
             write cannot be addressed to it. The storage mutate lock needs one to take over \
             an expired lease (see docs/operations.md)"
        );
    };
    let stale_version = UpdateVersion {
        e_tag: Some(e_tag),
        version: stale.version,
    };

    put(store, path, PROBE_BODY_REWRITE, PutMode::Overwrite).await?;

    match put(store, path, PROBE_BODY, PutMode::Update(stale_version)).await {
        // Exactly the error the lock's takeover reads as a lost race.
        Err(object_store::Error::Precondition { .. }) => Ok(()),
        Ok(_) => Err(anyhow::anyhow!(
            "{section} bucket `{bucket}` does not honor a conditional PUT's compare-and-swap: \
             an `If-Match` write against a superseded version succeeded instead of being \
             refused. On this backend two commands racing to take over one expired storage \
             mutate lock lease can both win, and both then rewrite the same Parquet partition. \
             Use a backend that implements `If-Match` (see docs/operations.md)"
        )),
        Err(e) => Err(anyhow::Error::new(e)),
    }
}

async fn put(
    store: &dyn ObjectStore,
    path: &ObjPath,
    body: &'static [u8],
    mode: PutMode,
) -> object_store::Result<PutResult> {
    store
        .put_opts(
            path,
            PutPayload::from_static(body),
            PutOptions {
                mode,
                ..Default::default()
            },
        )
        .await
}

/// Random per run, so concurrent starts probe independently and a leaked object
/// from an earlier run never passes for this run's first write.
fn probe_path(prefix: &str) -> ObjPath {
    obj_path(
        prefix,
        &format!("probe/conditional-put-{}", uuid::Uuid::new_v4()),
    )
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::sync::{Arc, Mutex, PoisonError};

    use futures::stream::{BoxStream, StreamExt};
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta,
        PutMultipartOptions, PutResult,
    };
    use rstest::rstest;

    use super::*;

    /// How the endpoint under test answers a conditional write.
    #[derive(Debug, Clone, Copy)]
    enum Endpoint {
        /// Honors every precondition, standing in for real S3.
        Conformant,
        /// Accepts every write whatever the `PutMode`.
        IgnoresMode,
        /// Honors `If-None-Match` but ignores `If-Match`.
        IgnoresSwap,
        /// Like S3 and unlike `InMemory`, derives each ETag from the content.
        ContentETag,
        /// Honors `If-None-Match` but returns no `ETag`.
        NoETag,
        /// What `object_store` answers when conditional put is configured off.
        Disabled,
        /// An operational fault that says nothing about conditional put.
        Fault,
    }

    /// How the endpoint answers the cleanup delete.
    #[derive(Debug, Clone, Copy)]
    enum Deletes {
        Succeeds,
        Missing,
        /// Refused, e.g. credentials without `s3:DeleteObject`.
        Fails,
    }

    /// Conformant preconditions come from `InMemory`; methods the probe must not
    /// call return `NotImplemented`, so an unexpected call fails it.
    #[derive(Debug)]
    struct FakeEndpoint {
        behavior: Endpoint,
        deletes: Deletes,
        inner: object_store::memory::InMemory,
        /// Content-derived ETag per key, for [`Endpoint::ContentETag`].
        content: Mutex<HashMap<ObjPath, String>>,
        /// Shared: the stream `delete_stream` returns cannot borrow the endpoint.
        deleted: Arc<Mutex<Vec<ObjPath>>>,
    }

    impl FakeEndpoint {
        fn new(behavior: Endpoint) -> Self {
            Self {
                behavior,
                deletes: Deletes::Succeeds,
                inner: object_store::memory::InMemory::new(),
                content: Mutex::new(HashMap::new()),
                deleted: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn with_deletes(mut self, deletes: Deletes) -> Self {
            self.deletes = deletes;
            self
        }

        fn deleted(&self) -> Vec<ObjPath> {
            self.deleted
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
        }

        fn not_implemented(&self, operation: &str) -> object_store::Error {
            object_store::Error::NotImplemented {
                operation: format!("`{operation}`"),
                implementer: self.to_string(),
            }
        }

        fn put_content_etag(
            &self,
            location: &ObjPath,
            payload: &PutPayload,
            mode: &PutMode,
        ) -> object_store::Result<PutResult> {
            let mut keys = self.content.lock().unwrap_or_else(PoisonError::into_inner);
            let current = keys.get(location).cloned();
            match mode {
                PutMode::Create if current.is_some() => {
                    return Err(object_store::Error::AlreadyExists {
                        path: location.to_string(),
                        source: "key exists".into(),
                    });
                }
                // Includes a missing key: S3 answers 404, which `object_store` remaps
                // to `Precondition`.
                PutMode::Update(v) if v.e_tag != current => {
                    return Err(object_store::Error::Precondition {
                        path: location.to_string(),
                        source: "etag mismatch".into(),
                    });
                }
                PutMode::Overwrite | PutMode::Create | PutMode::Update(_) => {}
            }
            let e_tag = etag_of(payload);
            keys.insert(location.clone(), e_tag.clone());
            Ok(PutResult {
                e_tag: Some(e_tag),
                version: None,
                extensions: Default::default(),
            })
        }
    }

    /// Stands in for S3's content-derived ETag: identical bytes, same ETag.
    fn etag_of(payload: &PutPayload) -> String {
        let mut hasher = DefaultHasher::new();
        payload
            .iter()
            .for_each(|chunk| chunk.as_ref().hash(&mut hasher));
        format!("\"{:x}\"", hasher.finish())
    }

    impl std::fmt::Display for FakeEndpoint {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "FakeEndpoint({:?})", self.behavior)
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for FakeEndpoint {
        async fn put_opts(
            &self,
            location: &ObjPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            match self.behavior {
                Endpoint::Conformant => self.inner.put_opts(location, payload, opts).await,
                Endpoint::IgnoresMode => Ok(PutResult {
                    e_tag: None,
                    version: None,
                    extensions: Default::default(),
                }),
                // `Update` lands as a plain overwrite.
                Endpoint::IgnoresSwap => {
                    let opts = match opts.mode {
                        PutMode::Update(_) => PutOptions {
                            mode: PutMode::Overwrite,
                            ..opts
                        },
                        PutMode::Overwrite | PutMode::Create => opts,
                    };
                    self.inner.put_opts(location, payload, opts).await
                }
                Endpoint::ContentETag => self.put_content_etag(location, &payload, &opts.mode),
                Endpoint::NoETag => {
                    let result = self.inner.put_opts(location, payload, opts).await?;
                    Ok(PutResult {
                        e_tag: None,
                        ..result
                    })
                }
                Endpoint::Disabled => Err(object_store::Error::NotImplemented {
                    operation: "`put_opts` with mode `PutMode::Create`".to_string(),
                    implementer: self.to_string(),
                }),
                Endpoint::Fault => Err(object_store::Error::Generic {
                    store: "FakeEndpoint",
                    source: format!("bucket policy denies PutObject on {location}").into(),
                }),
            }
        }

        async fn put_multipart_opts(
            &self,
            _location: &ObjPath,
            _opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            Err(self.not_implemented("put_multipart_opts"))
        }

        async fn get_opts(
            &self,
            _location: &ObjPath,
            _options: GetOptions,
        ) -> object_store::Result<GetResult> {
            Err(self.not_implemented("get_opts"))
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjPath>> {
            let deleted = Arc::clone(&self.deleted);
            let deletes = self.deletes;
            locations
                .map(move |location| {
                    let location = location?;
                    deleted
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(location.clone());
                    match deletes {
                        Deletes::Succeeds => Ok(location),
                        Deletes::Missing => Err(object_store::Error::NotFound {
                            path: location.to_string(),
                            source: "no such key".into(),
                        }),
                        Deletes::Fails => Err(object_store::Error::Generic {
                            store: "FakeEndpoint",
                            source: "delete denied".into(),
                        }),
                    }
                })
                .boxed()
        }

        fn list(
            &self,
            _prefix: Option<&ObjPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            futures::stream::iter([Err(self.not_implemented("list"))]).boxed()
        }

        async fn list_with_delimiter(
            &self,
            _prefix: Option<&ObjPath>,
        ) -> object_store::Result<ListResult> {
            Err(self.not_implemented("list_with_delimiter"))
        }

        async fn copy_opts(
            &self,
            _from: &ObjPath,
            _to: &ObjPath,
            _options: CopyOptions,
        ) -> object_store::Result<()> {
            Err(self.not_implemented("copy_opts"))
        }
    }

    /// A needle matching it proves the diagnosis interpolates the caller's text.
    const TEST_CONSEQUENCE: &str = "the probed mechanism breaks";

    /// `fatal`: a needle the probe's error must carry, or `None` to pass. The
    /// `Fault` needle is the endpoint's own message, so it proves propagation.
    #[rstest]
    #[case::conformant_create(Endpoint::Conformant, Requires::Create, None)]
    #[case::conformant_create_and_swap(Endpoint::Conformant, Requires::CreateAndSwap, None)]
    // Fails if the rewrite reuses `PROBE_BODY`; `Conformant` ETags ignore content.
    #[case::content_etag_create_and_swap(Endpoint::ContentETag, Requires::CreateAndSwap, None)]
    #[case::silently_unconditional_is_fatal(
        Endpoint::IgnoresMode,
        Requires::Create,
        Some("refused, so the probed mechanism breaks")
    )]
    #[case::ignored_swap_is_fatal(
        Endpoint::IgnoresSwap,
        Requires::CreateAndSwap,
        Some("does not honor a conditional PUT's compare-and-swap")
    )]
    #[case::ignored_swap_is_fine_without_it(Endpoint::IgnoresSwap, Requires::Create, None)]
    #[case::no_etag_is_fatal_for_a_swap(
        Endpoint::NoETag,
        Requires::CreateAndSwap,
        Some("returned no ETag")
    )]
    #[case::no_etag_is_fine_without_a_swap(Endpoint::NoETag, Requires::Create, None)]
    #[case::disabled_names_the_env_var(
        Endpoint::Disabled,
        Requires::Create,
        Some("AWS_CONDITIONAL_PUT")
    )]
    #[case::unrelated_fault_propagates(
        Endpoint::Fault,
        Requires::Create,
        Some("bucket policy denies PutObject")
    )]
    #[tokio::test]
    async fn probe_verdicts(
        #[case] endpoint: Endpoint,
        #[case] requires: Requires,
        #[case] fatal: Option<&str>,
    ) -> anyhow::Result<()> {
        let store: Arc<dyn ObjectStore> = Arc::new(FakeEndpoint::new(endpoint));
        let path = probe_path("some/prefix");
        let verdict = probe(
            &*store,
            &path,
            "[storage]",
            "test-bucket",
            TEST_CONSEQUENCE,
            requires,
        )
        .await;

        match fatal {
            None => verdict?,
            Some(needle) => {
                let Err(err) = verdict else {
                    anyhow::bail!("probe must be fatal");
                };
                let msg = err.to_string();
                assert!(
                    msg.contains(needle),
                    "error does not carry {needle:?}: {msg}"
                );
            }
        }
        Ok(())
    }

    #[rstest]
    #[case::under_a_prefix("root/tree", "root/tree/probe/")]
    #[case::at_the_root("", "probe/")]
    fn probe_path_sits_under_the_configured_prefix(#[case] prefix: &str, #[case] expected: &str) {
        let path = probe_path(prefix);
        assert!(
            path.as_ref().starts_with(expected),
            "prefix {prefix:?} produced {path}, expected it under {expected:?}"
        );
    }

    /// A failed or missing delete never changes the verdict.
    #[rstest]
    #[case::after_a_passing_probe(Endpoint::Conformant, Deletes::Succeeds, true)]
    #[case::after_a_fatal_probe(Endpoint::IgnoresMode, Deletes::Succeeds, false)]
    #[case::already_gone_is_tolerated(Endpoint::Conformant, Deletes::Missing, true)]
    #[case::a_refused_delete_keeps_the_verdict(Endpoint::Conformant, Deletes::Fails, true)]
    #[tokio::test]
    async fn cleanup_runs_whatever_the_verdict(
        #[case] endpoint: Endpoint,
        #[case] deletes: Deletes,
        #[case] passes: bool,
    ) -> anyhow::Result<()> {
        let store = FakeEndpoint::new(endpoint).with_deletes(deletes);
        let path = probe_path("some/prefix");
        let verdict = validate_store(
            &store,
            &path,
            "[storage]",
            "test-bucket",
            TEST_CONSEQUENCE,
            Requires::Create,
        )
        .await;

        assert_eq!(verdict.is_ok(), passes, "unexpected verdict: {verdict:?}");
        assert_eq!(
            store.deleted(),
            vec![path],
            "probe object was not cleaned up"
        );
        Ok(())
    }

    /// Else concurrent starts each lose the first `Create` and refuse to start.
    #[test]
    fn probe_path_is_randomized_per_call() {
        assert_ne!(probe_path(""), probe_path(""));
    }

    /// The bucket does not exist, so any request would fail the test.
    #[tokio::test]
    async fn disabled_probe_makes_no_request() -> anyhow::Result<()> {
        let storage = StorageConfig::S3 {
            bucket: "pipette-mgmt-nonexistent-probe-target".to_string(),
            prefix: String::new(),
            region: Some("us-east-1".to_string()),
            endpoint: None,
            max_concurrent_requests: 1,
            read_timeout_secs: None,
        };
        validate(
            false,
            &storage,
            "[storage]",
            TEST_CONSEQUENCE,
            Requires::CreateAndSwap,
        )
        .await
    }
}
