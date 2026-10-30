//! Copies every record from one [`Stores`] to another through the store traits.
//! `docs/cli.md` (`db import`) lists what it omits; it also skips the keys,
//! permissions and sign-in markers of a user whose record cannot be read.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Display;
use std::future::Future;

use anyhow::Context;
use chrono::{DateTime, Utc};
use futures::future::BoxFuture;
use futures::stream;
use futures::stream::{StreamExt, TryStreamExt};
use serde::Serialize;

use pipette_core::client::{Client, MigrationRecord};
use pipette_core::key_index::{KeyClaim, KeyHolderPrincipalId, PublicKeyDigest, digest_of};
use pipette_core::plan::PlanManifest;
use pipette_core::user::{User, UserKey};
use pipette_plan_types::{ClientId, ExpiresAt, JobId, OrgId, PlanId, UserId};
use pipette_store::{
    ClientListing, ContentKeyCas, ContentKeyClaim, ContentKeyRecord, ContentKeyVersion,
    CreateOutcome, EmailClaim, JobPlanMarker, STORAGE_CONCURRENCY, SlugClaim, Stores,
    UnreadableObjectError, drain_campaigns, drain_clients, drain_model_params, drain_plans,
    drain_vault_resources,
};

/// How [`copy_records`] treats the records that the target already holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// A claim or write-once record the target holds differently is a conflict,
    /// and nothing is deleted. `db import` copies this way.
    Merge,
    /// The source's claims and write-once records replace the target's, and target
    /// records the source lacks are deleted. `db export` copies this way.
    Mirror,
}

/// The result of [`copy_records`]. It holds one entry for each keyspace, in the
/// order that the copy ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyReport {
    pub mode: CopyMode,
    pub dry_run: bool,
    pub keyspaces: Vec<KeyspaceReport>,
}

impl CopyReport {
    pub fn written(&self) -> usize {
        self.keyspaces.iter().map(|k| k.written).sum()
    }

    pub fn deleted(&self) -> usize {
        self.keyspaces.iter().map(|k| k.deleted).sum()
    }

    pub fn conflicts(&self) -> usize {
        self.keyspaces.iter().map(|k| k.conflicts.len()).sum()
    }

    pub fn unreadable(&self) -> usize {
        self.keyspaces.iter().map(|k| k.unreadable.len()).sum()
    }
}

/// The count `read` equals `written + unchanged + skipped + conflicts`. It
/// excludes unreadable records and deleted target records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyspaceReport {
    pub keyspace: &'static str,
    pub read: usize,
    /// Records written to the target, or on a dry run that would be.
    pub written: usize,
    pub unchanged: usize,
    /// Target records the source lacks, deleted by a mirror (or that would be).
    pub deleted: usize,
    pub skipped: Vec<RecordNote>,
    pub unreadable: Vec<RecordNote>,
    pub conflicts: Vec<RecordNote>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordNote {
    pub id: String,
    pub reason: String,
}

impl KeyspaceReport {
    fn new(keyspace: &'static str) -> Self {
        Self {
            keyspace,
            read: 0,
            written: 0,
            unchanged: 0,
            deleted: 0,
            skipped: Vec::new(),
            unreadable: Vec::new(),
            conflicts: Vec::new(),
        }
    }

    fn record(&mut self, id: String, outcome: Outcome) {
        let note = |reason| RecordNote { id, reason };
        match outcome {
            Outcome::Written => self.written += 1,
            Outcome::Unchanged => self.unchanged += 1,
            Outcome::Skipped(reason) => self.skipped.push(note(reason)),
            Outcome::Conflict(reason) => self.conflicts.push(note(reason)),
            Outcome::Unreadable(reason) => {
                self.unreadable.push(note(reason));
                return;
            }
            Outcome::Deleted => {
                self.deleted += 1;
                return;
            }
        }
        self.read += 1;
    }

    fn unreadable_objects(&mut self, unreadable: Vec<UnreadableObjectError>) {
        self.unreadable
            .extend(unreadable.into_iter().map(|bad| RecordNote {
                id: bad.key,
                reason: bad.reason,
            }));
    }
}

enum Outcome {
    Written,
    Unchanged,
    Skipped(String),
    Unreadable(String),
    Conflict(String),
    /// A target record that the source does not hold, removed by a mirror copy.
    Deleted,
}

type Outcomes = Vec<(String, Outcome)>;

const SPENT_RECORD: &str = "its spent marker exists, and copying the record alone would make the \
                            key live again";
const SPENT_MARKER: &str = "no store method writes a spent marker; the key is not copied live";
const NAMES_A_TOKEN: &str = "the document names `auth_token`, and a store keeps no token";
const CHANGED_MID_COPY: &str = "the target's record changed during the copy";
const LISTING_SKIPPED: &str =
    "the source's listing skipped a record that reads; the target's copy stays";

/// Takes no lock, since no record writer takes one: stop the writers before a
/// cutover. A mirror deletes a target record only once a source read confirms
/// it gone, since a listing skips what it cannot read.
pub async fn copy_records(
    from: &Stores,
    to: &Stores,
    mode: CopyMode,
    dry_run: bool,
) -> anyhow::Result<CopyReport> {
    let run = Run {
        from,
        to,
        mode,
        dry_run,
    };
    let mut keyspaces = Vec::new();

    // Claims land before the records they protect, as registration writes them.
    let clients = drain_clients(&*from.clients)
        .await
        .context("listing clients")?;
    let users = from.users.list_users().await.context("listing users")?;
    let (user_keys, unreadable_keys) = read_user_keys(from, &users).await;
    let target = run.target_principals().await?;

    keyspaces.push(run.key_index(&clients.items, &user_keys, &target).await?);
    keyspaces.extend(run.clients(clients).await?);
    keyspaces.extend(
        run.users(users, user_keys, unreadable_keys, &target)
            .await?,
    );
    keyspaces.extend(run.orgs().await?);
    keyspaces.push(run.tags().await?);
    keyspaces.push(run.org_benchmarks().await?);
    keyspaces.extend(run.plans().await?);
    keyspaces.push(run.campaigns().await?);
    keyspaces.push(run.model_params().await?);
    keyspaces.push(run.vault().await?);
    Ok(CopyReport {
        mode,
        dry_run,
        keyspaces,
    })
}

/// A record id in two parts, shown as `first/second`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Pair<A, B>(A, B);

impl<A: Display, B: Display> Display for Pair<A, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.0, self.1)
    }
}

#[derive(Clone, Copy)]
struct Run<'a> {
    from: &'a Stores,
    to: &'a Stores,
    mode: CopyMode,
    dry_run: bool,
}

/// The target's principals before a mirror copy changes them, so the copy can
/// find the claims that only the target's records name. Empty for a merge.
#[derive(Default)]
struct TargetPrincipals {
    clients: Vec<Client>,
    users: Vec<User>,
    user_keys: Vec<(UserId, UserKey)>,
}

impl Run<'_> {
    fn mirror(self) -> bool {
        self.mode == CopyMode::Mirror
    }

    async fn target_principals(self) -> anyhow::Result<TargetPrincipals> {
        if !self.mirror() {
            return Ok(TargetPrincipals::default());
        }
        let clients = drain_clients(&*self.to.clients)
            .await
            .context("listing the target's clients")?
            .items;
        let users = self
            .to
            .users
            .list_users()
            .await
            .context("listing the target's users")?;
        let (user_keys, _) = read_user_keys(self.to, &users).await;
        Ok(TargetPrincipals {
            clients,
            users,
            user_keys,
        })
    }

    async fn delete<D>(self, delete: D) -> anyhow::Result<Outcome>
    where
        D: Future<Output = anyhow::Result<()>> + Send,
    {
        if !self.dry_run {
            delete.await?;
        }
        Ok(Outcome::Deleted)
    }

    /// Delete a target record the source's listing lacks, once `gone` confirms it.
    async fn delete_if_gone<G, D>(self, gone: G, delete: D) -> anyhow::Result<Outcome>
    where
        G: Future<Output = anyhow::Result<bool>> + Send,
        D: Future<Output = anyhow::Result<()>> + Send,
    {
        match gone.await {
            Ok(true) => self.delete(delete).await,
            Ok(false) => Ok(Outcome::Unreadable(LISTING_SKIPPED.to_owned())),
            Err(e) => Ok(unreadable(&e)),
        }
    }

    /// Make the target's claim on `name` match `want`, the source's holder (`None`
    /// for no claim), releasing and claiming only as needed.
    async fn settle<K: Claim>(
        self,
        name: &K::Name,
        want: Option<K::Holder>,
    ) -> anyhow::Result<Outcomes> {
        let to = self.to;
        let current = K::holder(to, name).await?;
        let outcome = match (want, current) {
            (None, None) => return Ok(Vec::new()),
            (None, Some(_)) if self.mirror() => self.delete(K::release(to, name)).await?,
            (None, Some(_)) => return Ok(Vec::new()),
            (Some(want), Some(current)) if want == current => Outcome::Unchanged,
            (Some(want), Some(current)) if !self.mirror() => held(&want, &current),
            (Some(_), _) if self.dry_run => Outcome::Written,
            (Some(want), current) => {
                if current.is_some() {
                    K::release(to, name).await?;
                }
                match K::claim(to, name, &want).await?.taken() {
                    None => Outcome::Written,
                    Some(holder) => held(&want, &holder),
                }
            }
        };
        Ok(one(name.to_string(), outcome))
    }

    async fn overwrite<W>(self, same: bool, write: W) -> anyhow::Result<Outcome>
    where
        W: Future<Output = anyhow::Result<()>> + Send,
    {
        if same {
            return Ok(Outcome::Unchanged);
        }
        if !self.dry_run {
            write.await?;
        }
        Ok(Outcome::Written)
    }

    async fn write_all<T, F, Fut>(self, items: Vec<T>, write: F) -> anyhow::Result<()>
    where
        T: Send,
        F: FnMut(T) -> Fut + Send,
        Fut: Future<Output = anyhow::Result<()>> + Send,
    {
        if self.dry_run {
            return Ok(());
        }
        stream::iter(items.into_iter().map(write))
            .buffer_unordered(STORAGE_CONCURRENCY)
            .try_collect::<Vec<()>>()
            .await?;
        Ok(())
    }

    /// Write each record the target lacks or holds differently, and return the
    /// ids in order.
    async fn copy_each<T, Id, C, G, GF, P, PF>(
        self,
        report: &mut KeyspaceReport,
        items: Vec<T>,
        id: fn(&T) -> Id,
        get: G,
        same: fn(&T, &C) -> anyhow::Result<bool>,
        put: P,
    ) -> anyhow::Result<Vec<Id>>
    where
        T: Send,
        Id: Display + Send,
        C: Send,
        G: Fn(Id) -> GF + Sync,
        GF: Future<Output = anyhow::Result<Option<C>>> + Send,
        P: Fn(T) -> PF + Sync,
        PF: Future<Output = anyhow::Result<()>> + Send,
    {
        let ids = items.iter().map(id).collect();
        let (get, put) = (&get, &put);
        each(report, items, |item| async move {
            let key = id(&item);
            let name = key.to_string();
            let unchanged = match get(key).await? {
                Some(current) => same(&item, &current)?,
                None => false,
            };
            let outcome = self.overwrite(unchanged, put(item)).await?;
            Ok(one(name, outcome))
        })
        .await?;
        Ok(ids)
    }

    /// Mirror only: delete each `target` record that `source` lacks, once `get`
    /// confirms it gone.
    async fn delete_stale<Id, L, S, G, GF, D, DF>(
        self,
        report: &mut KeyspaceReport,
        source: Vec<Id>,
        target: L,
        get: G,
        delete: D,
    ) -> anyhow::Result<()>
    where
        Id: Ord + Clone + Display + Send,
        L: Future<Output = anyhow::Result<Vec<Id>>>,
        G: Fn(Id) -> GF + Sync,
        GF: Future<Output = anyhow::Result<Option<S>>> + Send,
        D: Fn(Id) -> DF + Sync,
        DF: Future<Output = anyhow::Result<()>> + Send,
    {
        if !self.mirror() {
            return Ok(());
        }
        let stale = extra(source, target.await?);
        let (get, delete) = (&get, &delete);
        each(report, stale, |id| async move {
            let name = id.to_string();
            let read = get(id.clone());
            let gone = async { Ok(read.await?.is_none()) };
            let outcome = self.delete_if_gone(gone, delete(id)).await?;
            Ok(one(name, outcome))
        })
        .await?;
        Ok(())
    }

    async fn key_index(
        self,
        clients: &[Client],
        user_keys: &[(UserId, UserKey)],
        target: &TargetPrincipals,
    ) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("public-key-index");
        let claims = key_claims(clients, user_keys);
        let named: BTreeSet<PublicKeyDigest> = claims.iter().map(|(d, _)| d.clone()).collect();
        each(&mut report, claims, |(digest, record_holder)| async move {
            // The source's holder wins, so a keypair that two records name stays
            // with the principal that the source gave it to.
            let want = match self.from.key_index.holder_of_public_key(&digest).await {
                Ok(held) => held.unwrap_or(record_holder),
                Err(e) => return Ok(one(digest.to_string(), unreadable(&e))),
            };
            self.settle::<KeyClaim>(&digest, Some(want)).await
        })
        .await?;

        // Mirror only: the claims that only the target's records name.
        let stale = extra(
            named,
            key_claims(&target.clients, &target.user_keys)
                .into_iter()
                .map(|(digest, _)| digest),
        );
        each(&mut report, stale, |digest| async move {
            match self.from.key_index.holder_of_public_key(&digest).await {
                Ok(want) => self.settle::<KeyClaim>(&digest, want).await,
                Err(e) => Ok(one(digest.to_string(), unreadable(&e))),
            }
        })
        .await?;
        Ok(report)
    }

    /// Mirror only: the target's markers that the source lacks, reported as
    /// deleted. A marker carries no content, so a listing names every one.
    fn stale_markers<K: Ord + Clone>(
        self,
        report: &mut KeyspaceReport,
        source: &[K],
        target: &[K],
        id: impl Fn(&K) -> String,
    ) -> Vec<K> {
        if !self.mirror() {
            return Vec::new();
        }
        let stale = extra(source.iter().cloned(), target.iter().cloned());
        stale
            .iter()
            .for_each(|marker| report.record(id(marker), Outcome::Deleted));
        stale
    }

    async fn clients(self, listing: ClientListing) -> anyhow::Result<Vec<KeyspaceReport>> {
        let mut records = KeyspaceReport::new("clients");
        records.unreadable_objects(listing.unreadable);
        let source = self
            .copy_each(
                &mut records,
                listing.items,
                |c| c.client_id.clone(),
                |client_id| async move { self.to.clients.get_client(&client_id).await },
                same_json,
                |client| async move { self.to.clients.put_client(&client).await },
            )
            .await?;
        let target = async {
            let target = drain_clients(&*self.to.clients).await?.items;
            Ok(target.into_iter().map(|c| c.client_id).collect())
        };
        self.delete_stale(
            &mut records,
            source,
            target,
            |client_id| async move { self.from.clients.get_client(&client_id).await },
            |client_id| async move { self.to.clients.delete_client(&client_id).await },
        )
        .await?;

        let mut permissions = KeyspaceReport::new("client-permissions");
        let sets = self
            .from
            .clients
            .list_client_permissions()
            .await
            .context("listing client permissions")?;
        let source = self
            .copy_each(
                &mut permissions,
                sets,
                |(client_id, _)| client_id.clone(),
                |client_id| async move { self.to.clients.get_client_permissions(&client_id).await },
                |(_, set), current| Ok(set == current),
                |(client_id, set)| async move {
                    self.to
                        .clients
                        .put_client_permissions(&client_id, &set)
                        .await
                },
            )
            .await?;
        let target = async {
            let target = self.to.clients.list_client_permissions().await?;
            Ok(target.into_iter().map(|(client_id, _)| client_id).collect())
        };
        self.delete_stale(
            &mut permissions,
            source,
            target,
            |client_id| async move { self.from.clients.get_client_permissions(&client_id).await },
            |client_id| async move { self.to.clients.delete_client_permissions(&client_id).await },
        )
        .await?;

        let mut migrations = KeyspaceReport::new("signature-migration");
        let source = self
            .from
            .clients
            .list_signature_migrations()
            .await
            .context("listing signature migrations")?;
        let target: BTreeMap<ClientId, DateTime<Utc>> = self
            .to
            .clients
            .list_signature_migrations()
            .await?
            .into_iter()
            .map(|(id, m)| (id, m.first_seen))
            .collect();
        let target = &target;
        each(
            &mut migrations,
            source,
            |(client_id, migration)| async move {
                let first_seen = migration.first_seen;
                let outcome = match target.get(&client_id) {
                    Some(at) if *at == first_seen => Outcome::Unchanged,
                    Some(at) => Outcome::Conflict(format!(
                        "the target first saw it at {at}, the source at {first_seen}"
                    )),
                    None if self.dry_run => Outcome::Written,
                    None => match self
                        .to
                        .clients
                        .record_signature_migration(&client_id, first_seen)
                        .await?
                    {
                        MigrationRecord::First => Outcome::Written,
                        MigrationRecord::Existing => {
                            Outcome::Conflict("a marker appeared on the target mid-copy".to_owned())
                        }
                    },
                };
                Ok(one(client_id.to_string(), outcome))
            },
        )
        .await?;

        let mut keys = KeyspaceReport::new("client-preauth");
        let mut markers = KeyspaceReport::new("client-preauth spent markers");
        // Read the records before the markers, as prune does. A spend between
        // the two reads then appears as spent.
        let preauth = Preauth {
            mirror: self.mirror(),
            records: self
                .from
                .clients
                .list_client_preauth_keys()
                .await
                .context("listing client pre-auth keys")?,
            spent: self
                .from
                .clients
                .list_spent_client_preauth_markers()
                .await
                .context("listing spent client pre-auth markers")?
                .into_iter()
                .collect(),
            target: self
                .to
                .clients
                .list_client_preauth_keys()
                .await?
                .into_iter()
                .map(|k| (k.key_id.clone(), k))
                .collect(),
            target_spent: self
                .to
                .clients
                .list_spent_client_preauth_markers()
                .await?
                .into_iter()
                .collect(),
        };
        let decided = preauth.decide(|k| &k.key_id, &mut keys, &mut markers)?;
        self.write_all(decided.writes, |key| async move {
            self.to.clients.put_client_preauth_key(&key).await
        })
        .await?;
        self.write_all(decided.revoked, |key_id| async move {
            self.to.clients.delete_client_preauth_key(&key_id).await
        })
        .await?;
        self.write_all(decided.cleared_markers, |key_id| async move {
            self.to
                .clients
                .delete_spent_client_preauth_marker(&key_id)
                .await
        })
        .await?;

        Ok(vec![records, permissions, migrations, keys, markers])
    }

    async fn users(
        self,
        users: Vec<User>,
        user_keys: Vec<(UserId, UserKey)>,
        unreadable_keys: Vec<RecordNote>,
        target: &TargetPrincipals,
    ) -> anyhow::Result<Vec<KeyspaceReport>> {
        let ids: Vec<UserId> = users.iter().map(|u| u.user_id.clone()).collect();
        let target_ids: Vec<UserId> = target.users.iter().map(|u| u.user_id.clone()).collect();

        let mut claims = KeyspaceReport::new("user-email-index");
        let emails: Vec<String> = users
            .iter()
            .map(|u| u.contact_email.as_str().to_owned())
            .collect();
        each(&mut claims, users.clone(), |user| async move {
            let email = user.contact_email.as_str();
            let want = match self.from.users.lookup_user_by_email(email).await {
                Ok(held) => held.unwrap_or_else(|| user.user_id.clone()),
                Err(e) => return Ok(one(email.to_owned(), unreadable(&e))),
            };
            self.settle::<EmailClaim>(email, Some(want)).await
        })
        .await?;
        // Mirror only: the addresses that only the target's accounts name.
        let stale = extra(
            emails,
            target
                .users
                .iter()
                .map(|u| u.contact_email.as_str().to_owned()),
        );
        each(&mut claims, stale, |email| async move {
            match self.from.users.lookup_user_by_email(&email).await {
                Ok(want) => self.settle::<EmailClaim>(&email, want).await,
                Err(e) => Ok(one(email, unreadable(&e))),
            }
        })
        .await?;

        let mut records = KeyspaceReport::new("users");
        let source = self
            .copy_each(
                &mut records,
                users,
                |u| u.user_id.clone(),
                |user_id| async move { self.to.users.get_user(&user_id).await },
                |user, current| Ok(user == current),
                |user| async move { self.to.users.put_user(&user).await },
            )
            .await?;
        self.delete_stale(
            &mut records,
            source,
            async { Ok(target_ids.clone()) },
            |user_id| async move { self.from.users.get_user(&user_id).await },
            |user_id| async move { self.to.users.delete_user(&user_id).await },
        )
        .await?;

        let mut keys = KeyspaceReport::new("user-keys");
        keys.unreadable.extend(unreadable_keys);
        let key_id = |(user_id, key): &(UserId, UserKey)| Pair(user_id.clone(), key.key_id.clone());
        let source = self
            .copy_each(
                &mut keys,
                user_keys,
                key_id,
                |Pair(user_id, key_id)| async move {
                    self.to.users.get_user_key(&user_id, &key_id).await
                },
                |(_, key), current| Ok(key == current),
                |(user_id, key)| async move { self.to.users.put_user_key(&user_id, &key).await },
            )
            .await?;
        self.delete_stale(
            &mut keys,
            source,
            async { Ok(target.user_keys.iter().map(key_id).collect()) },
            |Pair(user_id, key_id)| async move {
                self.from.users.get_user_key(&user_id, &key_id).await
            },
            |Pair(user_id, key_id)| async move {
                self.to.users.delete_user_key(&user_id, &key_id).await
            },
        )
        .await?;

        let mut permissions = KeyspaceReport::new("user-permissions");
        each(&mut permissions, ids.clone(), |user_id| async move {
            let set = match self.from.users.get_user_permissions(&user_id).await {
                Ok(Some(set)) => set,
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Ok(one(user_id.to_string(), unreadable(&e))),
            };
            let current = self.to.users.get_user_permissions(&user_id).await?;
            let write = self.to.users.put_user_permissions(&user_id, &set);
            let outcome = self
                .overwrite(current.as_ref() == Some(&set), write)
                .await?;
            Ok(one(user_id.to_string(), outcome))
        })
        .await?;
        // Mirror only: a set the target holds for an account that has none on
        // the source.
        each(&mut permissions, target_ids.clone(), |user_id| async move {
            let users = (&*self.from.users, &*self.to.users);
            if users.1.get_user_permissions(&user_id).await?.is_none() {
                return Ok(Vec::new());
            }
            match users.0.get_user_permissions(&user_id).await {
                Ok(Some(_)) => Ok(Vec::new()),
                Ok(None) => {
                    let delete = users.1.delete_user_permissions(&user_id);
                    Ok(one(user_id.to_string(), self.delete(delete).await?))
                }
                Err(e) => Ok(one(user_id.to_string(), unreadable(&e))),
            }
        })
        .await?;

        let mut preauth_keys = KeyspaceReport::new("user-preauth");
        let mut markers = KeyspaceReport::new("user-preauth spent markers");
        let preauth = Preauth {
            mirror: self.mirror(),
            records: self
                .from
                .users
                .list_user_preauth_keys()
                .await
                .context("listing user pre-auth keys")?,
            spent: self
                .from
                .users
                .list_spent_user_preauth_markers()
                .await
                .context("listing spent user pre-auth markers")?
                .into_iter()
                .collect(),
            target: self
                .to
                .users
                .list_user_preauth_keys()
                .await?
                .into_iter()
                .map(|k| (k.key_id.clone(), k))
                .collect(),
            target_spent: self
                .to
                .users
                .list_spent_user_preauth_markers()
                .await?
                .into_iter()
                .collect(),
        };
        let decided = preauth.decide(|k| &k.key_id, &mut preauth_keys, &mut markers)?;
        self.write_all(decided.writes, |key| async move {
            self.to.users.put_user_preauth_key(&key).await
        })
        .await?;
        self.write_all(decided.revoked, |key_id| async move {
            self.to.users.delete_user_preauth_key(&key_id).await
        })
        .await?;
        self.write_all(decided.cleared_markers, |key_id| async move {
            self.to
                .users
                .delete_spent_user_preauth_marker(&key_id)
                .await
        })
        .await?;

        let mut awaiting = KeyspaceReport::new("user-awaiting-sign-in");
        each(&mut awaiting, ids.clone(), |user_id| async move {
            match self.from.users.user_awaits_sign_in(&user_id).await {
                Ok(true) => {}
                Ok(false) => return Ok(Vec::new()),
                Err(e) => return Ok(one(user_id.to_string(), unreadable(&e))),
            }
            let present = self.to.users.user_awaits_sign_in(&user_id).await?;
            let write = self.to.users.mark_user_awaiting_sign_in(&user_id);
            let outcome = self.overwrite(present, write).await?;
            Ok(one(user_id.to_string(), outcome))
        })
        .await?;
        // Mirror only: a marker the target holds for an account that has signed
        // in on the source.
        each(&mut awaiting, target_ids.clone(), |user_id| async move {
            let users = (&*self.from.users, &*self.to.users);
            if !users.1.user_awaits_sign_in(&user_id).await? {
                return Ok(Vec::new());
            }
            match users.0.user_awaits_sign_in(&user_id).await {
                Ok(true) => Ok(Vec::new()),
                Ok(false) => {
                    let delete = users.1.clear_user_awaiting_sign_in(&user_id);
                    Ok(one(user_id.to_string(), self.delete(delete).await?))
                }
                Err(e) => Ok(one(user_id.to_string(), unreadable(&e))),
            }
        })
        .await?;

        let mut first_sign_ins = KeyspaceReport::new("user-first-clerk-sign-in");
        each(&mut first_sign_ins, ids, |user_id| async move {
            let first = match self.from.users.get_user_first_clerk_sign_in(&user_id).await {
                Ok(Some(first)) => first,
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Ok(one(user_id.to_string(), unreadable(&e))),
            };
            let current = self.to.users.get_user_first_clerk_sign_in(&user_id).await?;
            // Exclusive create: a differing record is deleted before the write.
            let write = async {
                let users = &*self.to.users;
                users.delete_user_first_clerk_sign_in(&user_id).await?;
                users
                    .record_user_first_clerk_sign_in(&user_id, &first)
                    .await
                    .map(drop)
            };
            let outcome = self
                .overwrite(current.as_ref() == Some(&first), write)
                .await?;
            Ok(one(user_id.to_string(), outcome))
        })
        .await?;
        // Mirror only: a record the target holds for an account that has none
        // on the source.
        each(&mut first_sign_ins, target_ids, |user_id| async move {
            let users = (&*self.from.users, &*self.to.users);
            if users
                .1
                .get_user_first_clerk_sign_in(&user_id)
                .await?
                .is_none()
            {
                return Ok(Vec::new());
            }
            match users.0.get_user_first_clerk_sign_in(&user_id).await {
                Ok(Some(_)) => Ok(Vec::new()),
                Ok(None) => {
                    let delete = users.1.delete_user_first_clerk_sign_in(&user_id);
                    Ok(one(user_id.to_string(), self.delete(delete).await?))
                }
                Err(e) => Ok(one(user_id.to_string(), unreadable(&e))),
            }
        })
        .await?;

        Ok(vec![
            claims,
            records,
            keys,
            permissions,
            preauth_keys,
            markers,
            awaiting,
            first_sign_ins,
        ])
    }

    async fn orgs(self) -> anyhow::Result<Vec<KeyspaceReport>> {
        let orgs = self
            .from
            .orgs
            .list_orgs()
            .await
            .context("listing organizations")?;

        let target_orgs = if self.mirror() {
            self.to.orgs.list_orgs().await?
        } else {
            Vec::new()
        };

        let mut claims = KeyspaceReport::new("org-slug-index");
        let slugs: Vec<String> = orgs.iter().map(|o| o.slug.as_str().to_owned()).collect();
        each(&mut claims, orgs.clone(), |org| async move {
            let slug = org.slug.as_str();
            let want = match self.from.orgs.lookup_org_by_slug(slug).await {
                Ok(held) => held.unwrap_or_else(|| org.org_id.clone()),
                Err(e) => return Ok(one(slug.to_owned(), unreadable(&e))),
            };
            self.settle::<SlugClaim>(slug, Some(want)).await
        })
        .await?;
        // Mirror only: the handles that only the target's organizations name.
        let stale = extra(
            slugs,
            target_orgs.iter().map(|o| o.slug.as_str().to_owned()),
        );
        each(&mut claims, stale, |slug| async move {
            match self.from.orgs.lookup_org_by_slug(&slug).await {
                Ok(want) => self.settle::<SlugClaim>(&slug, want).await,
                Err(e) => Ok(one(slug, unreadable(&e))),
            }
        })
        .await?;

        let mut records = KeyspaceReport::new("orgs");
        let source = self
            .copy_each(
                &mut records,
                orgs,
                |o| o.org_id.clone(),
                |org_id| async move { self.to.orgs.get_org(&org_id).await },
                |org, current| Ok(org == current),
                |org| async move { self.to.orgs.put_org(&org).await },
            )
            .await?;
        self.delete_stale(
            &mut records,
            source,
            async { Ok(target_orgs.into_iter().map(|o| o.org_id).collect()) },
            |org_id| async move { self.from.orgs.get_org(&org_id).await },
            |org_id| async move { self.to.orgs.delete_org(&org_id).await },
        )
        .await?;

        let mut memberships = KeyspaceReport::new("user-orgs");
        let listed = self
            .from
            .orgs
            .list_memberships()
            .await
            .context("listing memberships")?;
        let source = self
            .copy_each(
                &mut memberships,
                listed,
                |m| m.user_id.clone(),
                |user_id| async move { self.to.orgs.get_membership(&user_id).await },
                |membership, org_id| Ok(membership.org_id == *org_id),
                |membership| async move { self.to.orgs.put_membership(&membership).await },
            )
            .await?;
        let target = async {
            let target = self.to.orgs.list_memberships().await?;
            Ok(target.into_iter().map(|m| m.user_id).collect())
        };
        self.delete_stale(
            &mut memberships,
            source,
            target,
            |user_id| async move { self.from.orgs.get_membership(&user_id).await },
            |user_id| async move { self.to.orgs.delete_membership(&user_id).await },
        )
        .await?;

        let mut ownerships = KeyspaceReport::new("client-owners");
        let listed = self
            .from
            .orgs
            .list_ownerships()
            .await
            .context("listing ownerships")?;
        let source = self
            .copy_each(
                &mut ownerships,
                listed,
                |o| o.client_id.clone(),
                |client_id| async move { self.to.orgs.get_ownership(&client_id).await },
                |ownership, owner| Ok(ownership.owner == *owner),
                |ownership| async move { self.to.orgs.put_ownership(&ownership).await },
            )
            .await?;
        let target = async {
            let target = self.to.orgs.list_ownerships().await?;
            Ok(target.into_iter().map(|o| o.client_id).collect())
        };
        self.delete_stale(
            &mut ownerships,
            source,
            target,
            |client_id| async move { self.from.orgs.get_ownership(&client_id).await },
            |client_id| async move { self.to.orgs.delete_ownership(&client_id).await },
        )
        .await?;

        Ok(vec![claims, records, memberships, ownerships])
    }

    /// The forward tree only: `add_client_tag` writes the reverse marker too.
    async fn tags(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("tags-index");
        let source = self
            .from
            .tags
            .list_forward_tag_markers()
            .await
            .context("listing tag markers")?;
        let target = self.to.tags.list_forward_tag_markers().await?;
        let stale = self.stale_markers(&mut report, &source, &target, |(client, tag)| {
            format!("{client}/{tag}")
        });
        let missing = missing(&mut report, source, target, |(client, tag)| {
            format!("{client}/{tag}")
        });
        self.write_all(missing, |(client_id, tag)| async move {
            self.to.tags.add_client_tag(&client_id, &tag).await
        })
        .await?;
        self.write_all(stale, |(client_id, tag)| async move {
            self.to.tags.remove_client_tag(&client_id, &tag).await
        })
        .await?;
        Ok(report)
    }

    /// The organizations' half of the catalog; the public half stays put.
    async fn org_benchmarks(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("org-benchmarks");
        let ids = self
            .from
            .catalog
            .list_org_benchmark_ids()
            .await
            .context("listing organization benchmarks")?;
        let source = ids.iter().cloned().map(|(o, b)| Pair(o, b)).collect();
        each(&mut report, ids, |(org_id, benchmark_id)| async move {
            let id = format!("{org_id}/{benchmark_id}");
            let benchmark = match self
                .from
                .catalog
                .get_org_benchmark(&org_id, &benchmark_id)
                .await
            {
                Ok(Some(benchmark)) => benchmark,
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Ok(one(id, unreadable(&e))),
            };
            let current = self
                .to
                .catalog
                .get_org_benchmark(&org_id, &benchmark_id)
                .await?;
            let write = self.to.catalog.put_org_benchmark(&org_id, &benchmark);
            let same = current.as_ref() == Some(&benchmark);
            Ok(one(id, self.overwrite(same, write).await?))
        })
        .await?;
        let target = async {
            let target = self.to.catalog.list_org_benchmark_ids().await?;
            Ok(target.into_iter().map(|(o, b)| Pair(o, b)).collect())
        };
        self.delete_stale(
            &mut report,
            source,
            target,
            |Pair(org_id, benchmark_id)| async move {
                self.from
                    .catalog
                    .get_org_benchmark(&org_id, &benchmark_id)
                    .await
            },
            |Pair(org_id, benchmark_id)| async move {
                self.to
                    .catalog
                    .delete_org_benchmark(&org_id, &benchmark_id)
                    .await
            },
        )
        .await?;
        Ok(report)
    }

    async fn plans(self) -> anyhow::Result<Vec<KeyspaceReport>> {
        let plans = &*self.from.plans;
        let manifests = drain_plans(plans, None).await.context("listing plans")?;
        let pending: Vec<PlanId> = plans
            .list_pending_source_markers()
            .await
            .context("listing pending plan-source markers")?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        let cancelled = plans
            .list_cancel_markers()
            .await
            .context("listing cancel markers")?;
        let content_keys = plans
            .list_content_keys()
            .await
            .context("listing content keys")?;
        let memberships = plans
            .list_all_job_plan_markers()
            .await
            .context("listing job-plan markers")?;
        let jobs = job_ids(&manifests.items, &memberships, &content_keys);
        // A source may outlive an ingest that wrote no manifest; its pending
        // marker is what names it then.
        let sourced: Vec<PlanId> = manifests
            .items
            .iter()
            .map(|m| m.plan_id.clone())
            .chain(pending.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();

        let mut pending_report = KeyspaceReport::new("plan-sources-pending");
        let target_pending: Vec<PlanId> = self
            .to
            .plans
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        let stale_pending = self.stale_markers(
            &mut pending_report,
            &pending,
            &target_pending,
            ToString::to_string,
        );
        let missing_pending = missing(
            &mut pending_report,
            pending,
            target_pending.clone(),
            ToString::to_string,
        );
        self.write_all(missing_pending, |plan_id| async move {
            self.to.plans.write_pending_source_marker(&plan_id).await
        })
        .await?;
        self.write_all(stale_pending, |plan_id| async move {
            self.to.plans.delete_pending_source_marker(&plan_id).await
        })
        .await?;

        let mut sources = KeyspaceReport::new("plan-sources");
        // Mirror only: a source the target keeps for an ingest that the source
        // has collected. Its pending marker is what names it.
        let stale_sources = if self.mirror() {
            extra(sourced.clone(), target_pending)
        } else {
            Vec::new()
        };
        each(&mut sources, sourced, |plan_id| async move {
            let id = plan_id.to_string();
            let source = match self.from.plans.get_plan_source(&plan_id).await {
                Ok(Some(source)) => source,
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Ok(one(id, unreadable(&e))),
            };
            if source.names_a_token() {
                return Ok(one(id, Outcome::Skipped(NAMES_A_TOKEN.to_owned())));
            }
            let outcome = match self.to.plans.get_plan_source(&plan_id).await? {
                Some(current) if current == source => Outcome::Unchanged,
                Some(_) if !self.mirror() => incumbent(false),
                Some(_) if self.dry_run => Outcome::Written,
                Some(_) => {
                    self.to.plans.delete_plan_source(&plan_id).await?;
                    match self.to.plans.put_plan_source(&plan_id, &source).await? {
                        CreateOutcome::Created => Outcome::Written,
                        CreateOutcome::AlreadyExists => incumbent(false),
                    }
                }
                None if self.dry_run => Outcome::Written,
                None => match self.to.plans.put_plan_source(&plan_id, &source).await? {
                    CreateOutcome::Created => Outcome::Written,
                    CreateOutcome::AlreadyExists => {
                        let current = self.to.plans.get_plan_source(&plan_id).await?;
                        incumbent(current.as_ref() == Some(&source))
                    }
                },
            };
            Ok(one(id, outcome))
        })
        .await?;
        each(&mut sources, stale_sources, |plan_id| async move {
            let plans = (&*self.from.plans, &*self.to.plans);
            if plans.1.get_plan_source(&plan_id).await?.is_none() {
                return Ok(Vec::new());
            }
            let gone = async { Ok(plans.0.get_plan_source(&plan_id).await?.is_none()) };
            let delete = plans.1.delete_plan_source(&plan_id);
            let outcome = self.delete_if_gone(gone, delete).await?;
            Ok(one(plan_id.to_string(), outcome))
        })
        .await?;

        let mut records = KeyspaceReport::new("plans");
        records.unreadable_objects(manifests.unreadable);
        self.copy_each(
            &mut records,
            manifests.items,
            |m| m.plan_id.clone(),
            |plan_id| async move { self.to.plans.get_plan(&plan_id).await },
            |manifest, current| Ok(manifest == current),
            |manifest| async move { self.to.plans.put_plan(&manifest).await },
        )
        .await?;

        let mut cancel = KeyspaceReport::new("cancelled_plans");
        let target = self.to.plans.list_cancel_markers().await?;
        let stale_cancel =
            self.stale_markers(&mut cancel, &cancelled, &target, ToString::to_string);
        let missing_cancel = missing(&mut cancel, cancelled, target, ToString::to_string);
        self.write_all(missing_cancel, |plan_id| async move {
            self.to.plans.write_cancel_marker(&plan_id).await
        })
        .await?;
        self.write_all(stale_cancel, |plan_id| async move {
            self.to.plans.delete_cancel_marker(&plan_id).await
        })
        .await?;

        let mut keys = KeyspaceReport::new("content-keys");
        let source_digests: Vec<String> = content_keys.iter().map(|(d, _, _)| d.clone()).collect();
        each(&mut keys, content_keys, |(digest, record, _)| async move {
            let outcome = match self.to.plans.get_content_key(&digest).await? {
                Some((current, _)) if current == record => Outcome::Unchanged,
                Some(_) if !self.mirror() => incumbent(false),
                Some(_) if self.dry_run => Outcome::Written,
                Some((_, version)) => {
                    match self
                        .to
                        .plans
                        .replace_content_key(&digest, &version, &record)
                        .await?
                    {
                        ContentKeyCas::Applied => Outcome::Written,
                        ContentKeyCas::Lost => Outcome::Conflict(CHANGED_MID_COPY.to_owned()),
                    }
                }
                None if self.dry_run => Outcome::Written,
                None => match self.to.plans.create_content_key(&digest, &record).await? {
                    ContentKeyClaim::Created => Outcome::Written,
                    ContentKeyClaim::AlreadyExists(current, _) => incumbent(current == record),
                },
            };
            Ok(one(digest, outcome))
        })
        .await?;
        if self.mirror() {
            let target = self.to.plans.list_content_keys().await?;
            let target_digests = target.iter().map(|(digest, _, _)| digest.clone());
            let stale: BTreeSet<String> =
                extra(source_digests, target_digests).into_iter().collect();
            let stale: Vec<(String, ContentKeyVersion)> = target
                .into_iter()
                .filter(|(digest, _, _)| stale.contains(digest))
                .map(|(digest, _, version)| (digest, version))
                .collect();
            each(&mut keys, stale, |(digest, version)| async move {
                let plans = (&*self.from.plans, &*self.to.plans);
                match plans.0.get_content_key(&digest).await {
                    Ok(None) => {}
                    Ok(Some(_)) => {
                        return Ok(one(digest, Outcome::Unreadable(LISTING_SKIPPED.to_owned())));
                    }
                    Err(e) => return Ok(one(digest, unreadable(&e))),
                }
                if self.dry_run {
                    return Ok(one(digest, Outcome::Deleted));
                }
                let outcome = match plans.1.delete_content_key(&digest, &version).await? {
                    ContentKeyCas::Applied => Outcome::Deleted,
                    ContentKeyCas::Lost => Outcome::Conflict(CHANGED_MID_COPY.to_owned()),
                };
                Ok(one(digest, outcome))
            })
            .await?;
        }

        let mut markers = KeyspaceReport::new("job-plans");
        let source: Vec<(JobId, PlanId, ExpiresAt)> = memberships
            .into_iter()
            .map(|m| (m.job_id, m.plan_id, m.expires_at))
            .collect();
        let target: Vec<(JobId, PlanId, ExpiresAt)> = self
            .to
            .plans
            .list_all_job_plan_markers()
            .await?
            .into_iter()
            .map(|m| (m.job_id, m.plan_id, m.expires_at))
            .collect();
        let marker_id = |(job, plan, at): &(JobId, PlanId, ExpiresAt)| format!("{job}/{plan}.{at}");
        let stale_markers = self.stale_markers(&mut markers, &source, &target, marker_id);
        let missing_markers = missing(&mut markers, source, target, marker_id);
        self.write_all(missing_markers, |(job_id, plan_id, at)| async move {
            self.to
                .plans
                .write_job_plan_marker(&job_id, &plan_id, at)
                .await
        })
        .await?;
        self.write_all(stale_markers, |(job_id, plan_id, at)| async move {
            self.to
                .plans
                .delete_job_plan_marker(&job_id, &plan_id, at)
                .await
        })
        .await?;

        let mut job_sources = KeyspaceReport::new("job-sources");
        each(&mut job_sources, jobs.clone(), |job_id| async move {
            let id = job_id.to_string();
            let body = match self.from.plans.get_job_source(&job_id).await {
                Ok(Some(body)) => body,
                Ok(None) => return Ok(Vec::new()),
                Err(e) => return Ok(one(id, unreadable(&e))),
            };
            let outcome = match self.to.plans.get_job_source(&job_id).await? {
                Some(current) => incumbent(current == body),
                None if self.dry_run => Outcome::Written,
                None => match self.to.plans.put_job_source(&job_id, &body).await? {
                    CreateOutcome::Created => Outcome::Written,
                    CreateOutcome::AlreadyExists => {
                        let current = self.to.plans.get_job_source(&job_id).await?;
                        incumbent(current.as_ref() == Some(&body))
                    }
                },
            };
            Ok(one(id, outcome))
        })
        .await?;

        let mut links = KeyspaceReport::new("job-plan-links");
        each(&mut links, jobs, |job_id| async move {
            let linked = match self.from.plans.list_job_plan_links(&job_id).await {
                Ok(linked) => linked,
                Err(e) => return Ok(one(job_id.to_string(), unreadable(&e))),
            };
            if linked.is_empty() {
                return Ok(Vec::new());
            }
            let present: BTreeSet<PlanId> = self
                .to
                .plans
                .list_job_plan_links(&job_id)
                .await?
                .into_iter()
                .collect();
            let mut outcomes = Vec::with_capacity(linked.len());
            for plan_id in linked {
                let outcome = if present.contains(&plan_id) {
                    Outcome::Unchanged
                } else if self.dry_run {
                    Outcome::Written
                } else {
                    match self.to.plans.put_job_plan_link(&job_id, &plan_id).await? {
                        CreateOutcome::Created => Outcome::Written,
                        CreateOutcome::AlreadyExists => Outcome::Unchanged,
                    }
                };
                outcomes.push((format!("{job_id}/{plan_id}"), outcome));
            }
            Ok(outcomes)
        })
        .await?;

        Ok(vec![
            pending_report,
            sources,
            records,
            cancel,
            keys,
            markers,
            job_sources,
            links,
        ])
    }

    async fn campaigns(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("campaigns");
        let listing = drain_campaigns(&*self.from.campaigns)
            .await
            .context("listing campaigns")?;
        report.unreadable_objects(listing.unreadable);
        let source = self
            .copy_each(
                &mut report,
                listing.items,
                |c| c.campaign_id.clone(),
                |campaign_id| async move { self.to.campaigns.get_campaign(&campaign_id).await },
                |campaign, current| Ok(campaign == current),
                |campaign| async move { self.to.campaigns.put_campaign(&campaign).await },
            )
            .await?;
        let target = async {
            let target = drain_campaigns(&*self.to.campaigns).await?.items;
            Ok(target.into_iter().map(|c| c.campaign_id).collect())
        };
        self.delete_stale(
            &mut report,
            source,
            target,
            |campaign_id| async move { self.from.campaigns.get_campaign(&campaign_id).await },
            |campaign_id| async move { self.to.campaigns.delete_campaign(&campaign_id).await },
        )
        .await?;
        Ok(report)
    }

    async fn model_params(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("model_params");
        let listing = drain_model_params(&*self.from.model_params)
            .await
            .context("listing model params")?;
        report.unreadable_objects(listing.unreadable);
        let source = self
            .copy_each(
                &mut report,
                listing.items,
                |p| p.key.clone(),
                |key| async move { self.to.model_params.get_model_params(&key).await },
                |params, current| Ok(params == current),
                |params| async move { self.to.model_params.put_model_params(&params).await },
            )
            .await?;
        let target = async {
            let target = drain_model_params(&*self.to.model_params).await?.items;
            Ok(target.into_iter().map(|p| p.key).collect())
        };
        self.delete_stale(
            &mut report,
            source,
            target,
            |key| async move { self.from.model_params.get_model_params(&key).await },
            |key| async move { self.to.model_params.delete_model_params(&key).await },
        )
        .await?;
        Ok(report)
    }

    /// The records only: the blobs they name stay where they are.
    async fn vault(self) -> anyhow::Result<KeyspaceReport> {
        let mut report = KeyspaceReport::new("vault");
        let listing = drain_vault_resources(&*self.from.vault)
            .await
            .context("listing vault resources")?;
        report.unreadable_objects(listing.unreadable);
        let source = self
            .copy_each(
                &mut report,
                listing.items,
                |r| r.resource_id.clone(),
                |resource_id| async move { self.to.vault.get_resource(&resource_id).await },
                |resource, current| Ok(resource == current),
                |resource| async move { self.to.vault.put_resource(&resource).await },
            )
            .await?;
        let target = async {
            let target = drain_vault_resources(&*self.to.vault).await?.items;
            Ok(target.into_iter().map(|r| r.resource_id).collect())
        };
        self.delete_stale(
            &mut report,
            source,
            target,
            |resource_id| async move { self.from.vault.get_resource(&resource_id).await },
            |resource_id| async move { self.to.vault.delete_resource(&resource_id).await },
        )
        .await?;
        Ok(report)
    }
}

async fn read_user_keys(
    stores: &Stores,
    users: &[User],
) -> (Vec<(UserId, UserKey)>, Vec<RecordNote>) {
    let ids: Vec<UserId> = users.iter().map(|u| u.user_id.clone()).collect();
    let listed: Vec<(UserId, anyhow::Result<Vec<UserKey>>)> = stream::iter(ids)
        .map(|user_id| async move {
            let keys = stores.users.list_user_keys(&user_id).await;
            (user_id, keys)
        })
        .buffered(STORAGE_CONCURRENCY)
        .collect()
        .await;
    listed.into_iter().fold(
        (Vec::new(), Vec::new()),
        |(mut keys, mut unreadable), (user_id, listing)| {
            match listing {
                Ok(found) => keys.extend(found.into_iter().map(|k| (user_id.clone(), k))),
                Err(e) => unreadable.push(RecordNote {
                    id: user_id.to_string(),
                    reason: format!("{e:#}"),
                }),
            }
            (keys, unreadable)
        },
    )
}

/// An index of exclusive claims, each name given to at most one holder.
trait Claim: Sized {
    type Name: Display + Sync + ?Sized;
    type Holder: PartialEq + Display + Send + Sync;

    fn holder<'s>(
        stores: &'s Stores,
        name: &'s Self::Name,
    ) -> BoxFuture<'s, anyhow::Result<Option<Self::Holder>>>;

    fn claim<'s>(
        stores: &'s Stores,
        name: &'s Self::Name,
        holder: &'s Self::Holder,
    ) -> BoxFuture<'s, anyhow::Result<Self>>;

    fn release<'s>(stores: &'s Stores, name: &'s Self::Name) -> BoxFuture<'s, anyhow::Result<()>>;

    /// The holder that already has the claim, if the claim did not succeed.
    fn taken(self) -> Option<Self::Holder>;
}

impl Claim for KeyClaim {
    type Name = PublicKeyDigest;
    type Holder = KeyHolderPrincipalId;

    fn holder<'s>(
        stores: &'s Stores,
        digest: &'s PublicKeyDigest,
    ) -> BoxFuture<'s, anyhow::Result<Option<KeyHolderPrincipalId>>> {
        stores.key_index.holder_of_public_key(digest)
    }

    fn claim<'s>(
        stores: &'s Stores,
        digest: &'s PublicKeyDigest,
        holder: &'s KeyHolderPrincipalId,
    ) -> BoxFuture<'s, anyhow::Result<Self>> {
        stores.key_index.claim_public_key(digest, holder)
    }

    fn release<'s>(
        stores: &'s Stores,
        digest: &'s PublicKeyDigest,
    ) -> BoxFuture<'s, anyhow::Result<()>> {
        stores.key_index.release_public_key(digest)
    }

    fn taken(self) -> Option<KeyHolderPrincipalId> {
        match self {
            KeyClaim::Claimed => None,
            KeyClaim::Taken(holder) => Some(holder),
        }
    }
}

impl Claim for EmailClaim {
    type Name = str;
    type Holder = UserId;

    fn holder<'s>(
        stores: &'s Stores,
        email: &'s str,
    ) -> BoxFuture<'s, anyhow::Result<Option<UserId>>> {
        stores.users.lookup_user_by_email(email)
    }

    fn claim<'s>(
        stores: &'s Stores,
        email: &'s str,
        holder: &'s UserId,
    ) -> BoxFuture<'s, anyhow::Result<Self>> {
        stores.users.claim_user_email(email, holder)
    }

    fn release<'s>(stores: &'s Stores, email: &'s str) -> BoxFuture<'s, anyhow::Result<()>> {
        stores.users.release_user_email(email)
    }

    fn taken(self) -> Option<UserId> {
        match self {
            EmailClaim::Claimed => None,
            EmailClaim::Taken(holder) => Some(holder),
        }
    }
}

impl Claim for SlugClaim {
    type Name = str;
    type Holder = OrgId;

    fn holder<'s>(
        stores: &'s Stores,
        slug: &'s str,
    ) -> BoxFuture<'s, anyhow::Result<Option<OrgId>>> {
        stores.orgs.lookup_org_by_slug(slug)
    }

    fn claim<'s>(
        stores: &'s Stores,
        slug: &'s str,
        holder: &'s OrgId,
    ) -> BoxFuture<'s, anyhow::Result<Self>> {
        stores.orgs.claim_org_slug(slug, holder)
    }

    fn release<'s>(stores: &'s Stores, slug: &'s str) -> BoxFuture<'s, anyhow::Result<()>> {
        stores.orgs.release_org_slug(slug)
    }

    fn taken(self) -> Option<OrgId> {
        match self {
            SlugClaim::Claimed => None,
            SlugClaim::Taken(holder) => Some(holder),
        }
    }
}

struct Preauth<Id, K> {
    mirror: bool,
    records: Vec<K>,
    spent: BTreeSet<Id>,
    target: BTreeMap<Id, K>,
    target_spent: BTreeSet<Id>,
}

/// What a copy does to the target's pre-auth keys.
struct PreauthChanges<Id, K> {
    writes: Vec<K>,
    /// Live on the target, and spent or gone on the source: mirror only.
    revoked: Vec<Id>,
    /// Spent markers that the source has pruned: mirror only.
    cleared_markers: Vec<Id>,
}

impl<Id: Ord + Display + Clone, K: Serialize> Preauth<Id, K> {
    fn decide(
        self,
        key_id: fn(&K) -> &Id,
        keys: &mut KeyspaceReport,
        markers: &mut KeyspaceReport,
    ) -> anyhow::Result<PreauthChanges<Id, K>> {
        let live: BTreeSet<Id> = self.records.iter().map(|k| key_id(k).clone()).collect();
        let writes = self
            .records
            .into_iter()
            .try_fold(Vec::new(), |mut writes, key| {
                let id = key_id(&key);
                let current = self.target.get(id);
                let outcome = if self.spent.contains(id) {
                    Outcome::Skipped(SPENT_RECORD.to_owned())
                } else if self.target_spent.contains(id) && !self.mirror {
                    // Spent on the target after an earlier copy, so the record has no use there;
                    // a mirror clears that marker below and writes it.
                    Outcome::Skipped("spent on the target".to_owned())
                } else if current.map(|c| same_json(&key, c)).transpose()? == Some(true) {
                    Outcome::Unchanged
                } else {
                    Outcome::Written
                };
                let write = matches!(outcome, Outcome::Written);
                keys.record(id.to_string(), outcome);
                if write {
                    writes.push(key);
                }
                anyhow::Ok(writes)
            })?;
        let mut revoked: Vec<Id> = self
            .spent
            .iter()
            .filter_map(|id| {
                // A key that became spent after an earlier copy is live on the
                // target.
                let live_on_target =
                    self.target.contains_key(id) && !self.target_spent.contains(id);
                let (outcome, revoke) = if live_on_target && self.mirror {
                    keys.record(id.to_string(), Outcome::Deleted);
                    (Outcome::Skipped(SPENT_MARKER.to_owned()), Some(id.clone()))
                } else if live_on_target {
                    let reason = "spent on the source but live on the target; revoke it";
                    (Outcome::Conflict(reason.to_owned()), None)
                } else {
                    (Outcome::Skipped(SPENT_MARKER.to_owned()), None)
                };
                markers.record(id.to_string(), outcome);
                revoke
            })
            .collect();
        let mut cleared_markers = Vec::new();
        if self.mirror {
            // Deleted on the source: neither live nor spent there.
            self.target
                .keys()
                .filter(|id| !live.contains(*id) && !self.spent.contains(*id))
                .for_each(|id| {
                    keys.record(id.to_string(), Outcome::Deleted);
                    revoked.push(id.clone());
                });
            self.target_spent
                .iter()
                .filter(|id| !self.spent.contains(*id))
                .for_each(|id| {
                    markers.record(id.to_string(), Outcome::Deleted);
                    cleared_markers.push(id.clone());
                });
        }
        Ok(PreauthChanges {
            writes,
            revoked,
            cleared_markers,
        })
    }
}

/// The keypair claim that each principal's record names.
fn key_claims(
    clients: &[Client],
    user_keys: &[(UserId, UserKey)],
) -> Vec<(PublicKeyDigest, KeyHolderPrincipalId)> {
    clients
        .iter()
        .map(|c| {
            let holder = KeyHolderPrincipalId::Client(c.client_id.clone());
            (digest_of(&c.public_key), holder)
        })
        .chain(user_keys.iter().map(|(user_id, key)| {
            let holder = KeyHolderPrincipalId::User(user_id.clone());
            (digest_of(&key.public_key), holder)
        }))
        .collect()
}

/// The `target` items that `source` lacks, once each, in order.
fn extra<K: Ord>(
    source: impl IntoIterator<Item = K>,
    target: impl IntoIterator<Item = K>,
) -> Vec<K> {
    let source: BTreeSet<K> = source.into_iter().collect();
    target
        .into_iter()
        .filter(|item| !source.contains(item))
        .collect::<BTreeSet<K>>()
        .into_iter()
        .collect()
}

/// Keeps input order in `report`, so a dry run's report equals the real run's.
///
/// The stream boxes each copy as it pulls the copy. The stream in
/// `record_each` then has one type, so its code compiles once for the crate.
/// A stream of unboxed copies has a different type at each call site, and
/// each call site compiles its own instance of the stream code.
async fn each<T, F, Fut>(
    report: &mut KeyspaceReport,
    items: Vec<T>,
    mut copy: F,
) -> anyhow::Result<()>
where
    T: Send,
    F: FnMut(T) -> Fut + Send,
    Fut: Future<Output = anyhow::Result<Outcomes>> + Send,
{
    record_each(
        report,
        Box::new(
            items
                .into_iter()
                .map(move |item| -> BoxFuture<'_, _> { Box::pin(copy(item)) }),
        ),
    )
    .await
}

async fn record_each(
    report: &mut KeyspaceReport,
    copies: Box<dyn Iterator<Item = BoxFuture<'_, anyhow::Result<Outcomes>>> + Send + '_>,
) -> anyhow::Result<()> {
    let outcomes: Vec<Outcomes> = stream::iter(copies)
        .buffered(STORAGE_CONCURRENCY)
        .try_collect()
        .await?;
    outcomes
        .into_iter()
        .flatten()
        .for_each(|(id, outcome)| report.record(id, outcome));
    Ok(())
}

/// Markers carry no content: one the target has is unchanged.
fn missing<K: Ord>(
    report: &mut KeyspaceReport,
    source: Vec<K>,
    target: impl IntoIterator<Item = K>,
    id: impl Fn(&K) -> String,
) -> Vec<K> {
    let target: BTreeSet<K> = target.into_iter().collect();
    source
        .into_iter()
        .filter(|marker| {
            let present = target.contains(marker);
            let outcome = if present {
                Outcome::Unchanged
            } else {
                Outcome::Written
            };
            report.record(id(marker), outcome);
            !present
        })
        .collect()
}

/// Job sources and links have no listing. The copy misses a job that no
/// readable manifest, `job-plans/` marker, or content key names.
fn job_ids(
    manifests: &[PlanManifest],
    memberships: &[JobPlanMarker],
    content_keys: &[(String, ContentKeyRecord, ContentKeyVersion)],
) -> Vec<JobId> {
    manifests
        .iter()
        .flat_map(|m| m.job_ids.iter().chain(m.rerun_of.values()))
        .chain(memberships.iter().map(|m| &m.job_id))
        .chain(content_keys.iter().map(|(_, record, _)| &record.job_id))
        .cloned()
        .collect::<BTreeSet<JobId>>()
        .into_iter()
        .collect()
}

fn one(id: String, outcome: Outcome) -> Outcomes {
    vec![(id, outcome)]
}

fn unreadable(e: &anyhow::Error) -> Outcome {
    Outcome::Unreadable(format!("{e:#}"))
}

/// An exclusive claim that the target holds for a holder other than `want`.
fn held<H: Display>(want: &H, holder: &H) -> Outcome {
    Outcome::Conflict(format!("the target holds it for {holder}, not {want}"))
}

/// A create-once record the target already holds.
fn incumbent(same: bool) -> Outcome {
    if same {
        Outcome::Unchanged
    } else {
        Outcome::Conflict("the target holds a different record".to_owned())
    }
}

/// Equality through the stored form, for the records that derive no `PartialEq`.
fn same_json<T: Serialize>(a: &T, b: &T) -> anyhow::Result<bool> {
    Ok(serde_json::to_value(a)? == serde_json::to_value(b)?)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use pipette_core::campaign::{CampaignOwner, NewCampaign};
    use pipette_core::client::ClientStatus;
    use pipette_core::client_permission::ClientPermissionSet;
    use pipette_core::client_preauth;
    use pipette_core::client_preauth::{
        ClientMintedKey, ClientPreauthConsumeOutcome, ClientPreauthParams,
    };
    use pipette_core::organization::{Membership, Organization, Ownership};
    use pipette_core::plan::{PlanSource, sample_manifest};
    use pipette_core::types::{DeriveFromKey, Mint};
    use pipette_core::user_permission::UserPermissionSet;
    use pipette_core::user_preauth;
    use pipette_core::user_preauth::{UserPreauthConsumeOutcome, UserPreauthParams};
    use pipette_plan_types::{
        BenchmarkId, CampaignId, ClientOwner, ClientPermission, ClientPreauthKeyId,
        ClientPreauthUsage, ContactEmail, ExpiresAt, FirstClerkSignIn, NonEmptyTrimmedString,
        OrgId, OrgSlug, PlanStatus, PublicKeyHex, Tag, UserPermission, UserPreauthKeyId,
        UserPreauthUsage, VaultResourceId,
    };
    use pipette_store_fs::build_local_fs_stores;

    use super::*;

    fn stores() -> anyhow::Result<(tempfile::TempDir, Stores)> {
        let dir = tempfile::tempdir()?;
        let config = pipette_core::config::Config {
            storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            auth_storage: pipette_core::config::StorageConfig::local_fs(dir.path().to_path_buf()),
            ..Default::default()
        };
        let stores = build_local_fs_stores(&config)?;
        Ok((dir, stores))
    }

    struct Seeded {
        client_id: ClientId,
        client_digest: PublicKeyDigest,
        user: User,
        user_key: UserKey,
        org: Organization,
        tag: Tag,
        benchmark_id: BenchmarkId,
        plan_id: PlanId,
        /// A source only its pending marker names: an ingest that stopped
        /// before its manifest.
        orphan_plan_id: PlanId,
        /// `job-0` is named only by the manifest's `rerun_of`.
        jobs: Vec<JobId>,
        content_digest: String,
        campaign_id: CampaignId,
        resource_id: VaultResourceId,
        live_client_key: ClientPreauthKeyId,
        /// Consumed: the marker stands and the record is gone.
        spent_client_key: ClientPreauthKeyId,
        /// A consume that stopped before its delete: marker and record both.
        half_spent_client_key: ClientPreauthKeyId,
        spent_user_key: UserPreauthKeyId,
    }

    fn client_params(usage: ClientPreauthUsage) -> ClientPreauthParams {
        ClientPreauthParams {
            usage,
            expires_at: None,
            default_tags: BTreeSet::new(),
            default_organization: None,
            default_owner: None,
            default_permissions: None,
            note: None,
        }
    }

    async fn spend(
        stores: &Stores,
        minted: &ClientMintedKey,
    ) -> anyhow::Result<ClientPreauthKeyId> {
        stores.clients.put_client_preauth_key(&minted.key).await?;
        let (key_id, secret) = minted
            .token
            .parse_parts()
            .context("a minted token parses")?;
        let consumed = stores
            .clients
            .consume_client_preauth_key(&key_id, &secret)
            .await?;
        anyhow::ensure!(matches!(consumed, ClientPreauthConsumeOutcome::Granted(_)));
        Ok(key_id)
    }

    fn user_params(user_id: &UserId, usage: UserPreauthUsage) -> anyhow::Result<UserPreauthParams> {
        Ok(UserPreauthParams {
            user_id: user_id.clone(),
            usage,
            expires_at: None,
            note: Some(NonEmptyTrimmedString::try_new("laptop")?),
        })
    }

    /// One record of every kind, written in the order their writers use.
    async fn seed(stores: &Stores) -> anyhow::Result<Seeded> {
        let now = Utc::now();

        let public_key = PublicKeyHex::try_new(hex::encode([0x11; PublicKeyHex::BYTE_LEN]))?;
        let client = Client {
            client_id: ClientId::derive(&public_key),
            public_key: public_key.clone(),
            organization: NonEmptyTrimmedString::try_new("lab")?,
            client_details: NonEmptyTrimmedString::try_new("rig")?,
            contact_email: ContactEmail::try_new("rig@example.test")?,
            status: ClientStatus::Approved,
            registered_at: now,
            device_profile: Default::default(),
            capabilities: BTreeSet::from(["runtime:llama_cpp".to_owned()]),
        };
        let client_digest = digest_of(&public_key);
        let holder = KeyHolderPrincipalId::Client(client.client_id.clone());
        stores
            .key_index
            .claim_public_key(&client_digest, &holder)
            .await?;
        let set = ClientPermissionSet::from_iter([ClientPermission::PerformWork]);
        stores
            .clients
            .put_client_permissions(&client.client_id, &set)
            .await?;
        stores.clients.put_client(&client).await?;
        // Nanoseconds, so a backend that truncates the first sighting shows.
        let first_seen = Utc
            .timestamp_opt(1_790_000_000, 123_456_789)
            .single()
            .context("a valid instant")?;
        stores
            .clients
            .record_signature_migration(&client.client_id, first_seen)
            .await?;

        let live = client_preauth::mint(client_params(ClientPreauthUsage::MultiUse), now)?;
        stores.clients.put_client_preauth_key(&live.key).await?;
        let single_use = || client_preauth::mint(client_params(ClientPreauthUsage::SingleUse), now);
        let spent_client_key = spend(stores, &single_use()?).await?;
        let half_spent = single_use()?;
        let half_spent_client_key = spend(stores, &half_spent).await?;
        stores
            .clients
            .put_client_preauth_key(&half_spent.key)
            .await?;

        let user = User::try_new(ContactEmail::try_new("op@example.test")?, now)?;
        stores
            .users
            .claim_user_email(user.contact_email.as_str(), &user.user_id)
            .await?;
        stores.users.put_user(&user).await?;
        let user_public_key = PublicKeyHex::try_new(hex::encode([0x22; PublicKeyHex::BYTE_LEN]))?;
        let user_key = UserKey::new(
            user_public_key.clone(),
            NonEmptyTrimmedString::try_new("laptop")?,
            now,
        );
        let holder = KeyHolderPrincipalId::User(user.user_id.clone());
        stores
            .key_index
            .claim_public_key(&digest_of(&user_public_key), &holder)
            .await?;
        stores.users.put_user_key(&user.user_id, &user_key).await?;
        let grants = UserPermissionSet::from_iter([UserPermission::SubmitPlans]);
        stores
            .users
            .put_user_permissions(&user.user_id, &grants)
            .await?;
        stores
            .users
            .mark_user_awaiting_sign_in(&user.user_id)
            .await?;
        let first = FirstClerkSignIn {
            at: now,
            prior_keys: vec![user_key.key_id.clone()],
        };
        stores
            .users
            .record_user_first_clerk_sign_in(&user.user_id, &first)
            .await?;
        let live_user =
            user_preauth::mint(user_params(&user.user_id, UserPreauthUsage::MultiUse)?, now)?;
        stores.users.put_user_preauth_key(&live_user.key).await?;
        let spent_user = user_preauth::mint(
            user_params(&user.user_id, UserPreauthUsage::SingleUse)?,
            now,
        )?;
        stores.users.put_user_preauth_key(&spent_user.key).await?;
        let (spent_user_key, secret) = spent_user
            .token
            .parse_parts()
            .context("a minted token parses")?;
        let consumed = stores
            .users
            .consume_user_preauth_key(&spent_user_key, &secret)
            .await?;
        anyhow::ensure!(matches!(consumed, UserPreauthConsumeOutcome::Granted(_)));

        let org = Organization::new(
            OrgSlug::try_new("acme")?,
            NonEmptyTrimmedString::try_new("Acme")?,
            now,
        );
        stores
            .orgs
            .claim_org_slug(org.slug.as_str(), &org.org_id)
            .await?;
        stores.orgs.put_org(&org).await?;
        stores
            .orgs
            .put_membership(&Membership {
                user_id: user.user_id.clone(),
                org_id: org.org_id.clone(),
            })
            .await?;
        stores
            .orgs
            .put_ownership(&Ownership {
                client_id: client.client_id.clone(),
                owner: ClientOwner::Org {
                    org_id: org.org_id.clone(),
                },
            })
            .await?;
        let tag = Tag::try_new("lab")?;
        stores.tags.add_client_tag(&client.client_id, &tag).await?;
        let benchmark = pipette_store_testkit::an_org_benchmark()?;
        stores
            .catalog
            .put_org_benchmark(&org.org_id, &benchmark)
            .await?;

        let plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        let manifest = sample_manifest(plan_id.clone(), PlanStatus::Active, true)?;
        let source = PlanSource::new("name = \"smoke\"\n".to_owned())?;
        stores.plans.put_plan_source(&plan_id, &source).await?;
        stores.plans.put_plan(&manifest).await?;
        stores.plans.write_cancel_marker(&plan_id).await?;
        let orphan_plan_id = PlanId::from_uuid(uuid::Uuid::now_v7());
        stores
            .plans
            .write_pending_source_marker(&orphan_plan_id)
            .await?;
        let orphan = PlanSource::new("orphan".to_owned())?;
        stores
            .plans
            .put_plan_source(&orphan_plan_id, &orphan)
            .await?;
        let jobs: Vec<JobId> = ["job-0", "job-1", "job-2"]
            .into_iter()
            .map(JobId::new_unchecked)
            .collect();
        for job_id in &jobs {
            let body = serde_json::json!({"job_id": job_id, "benchmark_id": "smoke", "ratio": 0.1});
            stores.plans.put_job_source(job_id, &body).await?;
            stores.plans.put_job_plan_link(job_id, &plan_id).await?;
        }
        for job_id in &manifest.job_ids {
            stores
                .plans
                .write_job_plan_marker(job_id, &plan_id, ExpiresAt::Never)
                .await?;
        }
        let content_digest = "a".repeat(64);
        let record = ContentKeyRecord {
            job_id: JobId::new_unchecked("job-1"),
            plan_id: plan_id.clone(),
            created_at: now,
        };
        stores
            .plans
            .create_content_key(&content_digest, &record)
            .await?;

        let campaign = NewCampaign::new(
            NonEmptyTrimmedString::try_new("Nightly")?,
            CampaignOwner::Org {
                org_id: org.org_id.clone(),
            },
        )
        .around(plan_id.clone(), now);
        stores.campaigns.put_campaign(&campaign).await?;
        let resource = pipette_store_testkit::a_vault_resource()?;
        stores.vault.put_resource(&resource).await?;
        stores
            .model_params
            .put_model_params(&pipette_store_testkit::a_model_params(
                "acmelm-4.5b-text",
                4512,
            )?)
            .await?;

        Ok(Seeded {
            client_id: client.client_id,
            client_digest,
            user,
            user_key,
            org,
            tag,
            benchmark_id: benchmark.benchmark_id().clone(),
            plan_id,
            orphan_plan_id,
            jobs,
            content_digest,
            campaign_id: campaign.campaign_id,
            resource_id: resource.resource_id,
            live_client_key: live.key.key_id,
            spent_client_key,
            half_spent_client_key,
            spent_user_key,
        })
    }

    /// Only the live pre-auth keys: the spent ones are not meant to cross.
    async fn snapshot(stores: &Stores, s: &Seeded) -> anyhow::Result<serde_json::Value> {
        let (clients, users, orgs, plans) = (
            &*stores.clients,
            &*stores.users,
            &*stores.orgs,
            &*stores.plans,
        );
        let user_id = &s.user.user_id;
        let mut migrations: Vec<(ClientId, DateTime<Utc>)> = clients
            .list_signature_migrations()
            .await?
            .into_iter()
            .map(|(id, m)| (id, m.first_seen))
            .collect();
        migrations.sort();
        let mut user_preauth: Vec<serde_json::Value> = users
            .list_user_preauth_keys()
            .await?
            .iter()
            .map(serde_json::to_value)
            .collect::<Result<_, _>>()?;
        user_preauth.sort_by_key(ToString::to_string);
        let mut pending: Vec<PlanId> = plans
            .list_pending_source_markers()
            .await?
            .into_iter()
            .map(|m| m.plan_id)
            .collect();
        pending.sort();
        let mut markers: Vec<String> = plans
            .list_all_job_plan_markers()
            .await?
            .into_iter()
            .map(|m| format!("{}/{}.{}", m.job_id, m.plan_id, m.expires_at))
            .collect();
        markers.sort();
        let mut job_sources = Vec::new();
        let mut links = Vec::new();
        for job_id in &s.jobs {
            job_sources.push(plans.get_job_source(job_id).await?);
            links.push(plans.list_job_plan_links(job_id).await?);
        }
        let mut sources = Vec::new();
        for plan_id in [&s.plan_id, &s.orphan_plan_id] {
            let source = plans.get_plan_source(plan_id).await?;
            sources.push(source.map(|s| s.text().to_owned()));
        }
        let user_key_digest = digest_of(&s.user_key.public_key);
        let content_key = plans.get_content_key(&s.content_digest).await?;
        Ok(serde_json::json!({
            "client": clients.get_client(&s.client_id).await?,
            "client_permissions": clients.get_client_permissions(&s.client_id).await?,
            "client_claim": stores.key_index.holder_of_public_key(&s.client_digest).await?,
            "signature_migrations": migrations,
            "client_preauth": clients.get_client_preauth_key(&s.live_client_key).await?,
            "user": users.get_user(user_id).await?,
            "email_claim": users.lookup_user_by_email(s.user.contact_email.as_str()).await?,
            "user_keys": users.list_user_keys(user_id).await?,
            "user_claim": stores.key_index.holder_of_public_key(&user_key_digest).await?,
            "user_permissions": users.get_user_permissions(user_id).await?,
            "user_preauth": user_preauth,
            "awaiting_sign_in": users.user_awaits_sign_in(user_id).await?,
            "first_clerk_sign_in": users.get_user_first_clerk_sign_in(user_id).await?,
            "org": orgs.get_org(&s.org.org_id).await?,
            "slug_claim": orgs.lookup_org_by_slug(s.org.slug.as_str()).await?,
            "membership": orgs.get_membership(user_id).await?,
            "ownership": orgs.get_ownership(&s.client_id).await?,
            "tags": stores.tags.get_client_tags(&s.client_id).await?,
            "tagged": stores.tags.list_client_ids_by_tag(&s.tag).await?,
            "org_benchmark": stores
                .catalog
                .get_org_benchmark(&s.org.org_id, &s.benchmark_id)
                .await?,
            "plan": plans.get_plan(&s.plan_id).await?,
            "cancel_marker": plans.has_cancel_marker(&s.plan_id).await?,
            "plan_sources": sources,
            "pending_markers": pending,
            "content_key": content_key.map(|(record, _)| record),
            "job_plan_markers": markers,
            "job_sources": job_sources,
            "job_plan_links": links,
            "campaign": stores.campaigns.get_campaign(&s.campaign_id).await?,
            "model_params": drain_model_params(&*stores.model_params).await?.items,
            "vault_resource": stores.vault.get_resource(&s.resource_id).await?,
        }))
    }

    fn keyspace<'r>(report: &'r CopyReport, name: &str) -> anyhow::Result<&'r KeyspaceReport> {
        report
            .keyspaces
            .iter()
            .find(|k| k.keyspace == name)
            .with_context(|| format!("no {name} keyspace in the report"))
    }

    fn ids(notes: &[RecordNote]) -> BTreeSet<&str> {
        notes.iter().map(|n| n.id.as_str()).collect()
    }

    /// Files at any depth: the directories the builder makes hold none.
    fn files_under(dir: &std::path::Path) -> anyhow::Result<Vec<std::path::PathBuf>> {
        let mut files = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                files.extend(files_under(&path)?);
            } else {
                files.push(path);
            }
        }
        Ok(files)
    }

    #[tokio::test]
    async fn every_record_reads_back_equal_and_a_second_run_writes_nothing() -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;

        let first = copy_records(&from, &to, CopyMode::Merge, false).await?;
        assert_eq!(first.conflicts(), 0, "{first:#?}");
        assert_eq!(first.unreadable(), 0, "{first:#?}");
        // A keyspace the seed missed would pass everything below vacuously.
        let unseeded: Vec<&str> = first
            .keyspaces
            .iter()
            .filter(|k| k.read == 0)
            .map(|k| k.keyspace)
            .collect();
        assert!(unseeded.is_empty(), "nothing seeded in {unseeded:?}");
        assert_eq!(
            snapshot(&to, &seeded).await?,
            snapshot(&from, &seeded).await?
        );

        let second = copy_records(&from, &to, CopyMode::Merge, false).await?;
        assert_eq!(second.written(), 0, "{second:#?}");
        assert_eq!(second.conflicts(), 0, "{second:#?}");
        for (again, before) in second.keyspaces.iter().zip(&first.keyspaces) {
            assert_eq!(again.read, before.read, "{}", again.keyspace);
            assert_eq!(
                again.unchanged,
                before.written + before.unchanged,
                "{}",
                again.keyspace
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn a_spent_single_use_key_is_skipped_not_copied_live() -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;

        let report = copy_records(&from, &to, CopyMode::Merge, false).await?;
        let keys = keyspace(&report, "client-preauth")?;
        assert_eq!(
            ids(&keys.skipped),
            BTreeSet::from([seeded.half_spent_client_key.as_str()])
        );
        assert_eq!(keys.written, 1, "the live key alone");
        let markers = keyspace(&report, "client-preauth spent markers")?;
        assert_eq!(
            ids(&markers.skipped),
            BTreeSet::from([
                seeded.spent_client_key.as_str(),
                seeded.half_spent_client_key.as_str(),
            ])
        );
        let user_markers = keyspace(&report, "user-preauth spent markers")?;
        assert_eq!(
            ids(&user_markers.skipped),
            BTreeSet::from([seeded.spent_user_key.as_str()])
        );
        for key_id in [&seeded.spent_client_key, &seeded.half_spent_client_key] {
            assert_eq!(to.clients.get_client_preauth_key(key_id).await?, None);
        }
        assert_eq!(to.users.list_user_preauth_keys().await?.len(), 1);

        // Spent on the source after a copy, the key is still live on the
        // target: the copy cannot fix that, so it must not read as skipped.
        let late = client_preauth::mint(client_params(ClientPreauthUsage::SingleUse), Utc::now())?;
        from.clients.put_client_preauth_key(&late.key).await?;
        copy_records(&from, &to, CopyMode::Merge, false).await?;
        let late_id = spend(&from, &late).await?;
        let report = copy_records(&from, &to, CopyMode::Merge, false).await?;
        let markers = keyspace(&report, "client-preauth spent markers")?;
        assert_eq!(ids(&markers.conflicts), BTreeSet::from([late_id.as_str()]));
        Ok(())
    }

    #[tokio::test]
    async fn a_claim_another_principal_holds_is_a_conflict_and_the_rest_still_copies()
    -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;
        let squatter = UserId::mint();
        to.users
            .claim_user_email(seeded.user.contact_email.as_str(), &squatter)
            .await?;
        to.orgs.claim_org_slug("acme", &OrgId::mint()).await?;
        to.key_index
            .claim_public_key(&seeded.client_digest, &KeyHolderPrincipalId::User(squatter))
            .await?;

        let report = copy_records(&from, &to, CopyMode::Merge, false).await?;
        let conflicted: BTreeSet<&str> = report
            .keyspaces
            .iter()
            .filter(|k| !k.conflicts.is_empty())
            .map(|k| k.keyspace)
            .collect();
        assert_eq!(
            conflicted,
            BTreeSet::from(["public-key-index", "user-email-index", "org-slug-index"])
        );
        assert_eq!(
            ids(&keyspace(&report, "user-email-index")?.conflicts),
            BTreeSet::from([seeded.user.contact_email.as_str()])
        );
        assert_eq!(
            to.users.get_user(&seeded.user.user_id).await?.as_ref(),
            Some(&seeded.user)
        );
        assert!(to.plans.get_plan(&seeded.plan_id).await?.is_some());
        Ok(())
    }

    /// `job-plan-links/` has no delete, and the source never deletes a link, so a
    /// mirror copy leaves the target's extra links in place.
    async fn mirrored(stores: &Stores, s: &Seeded) -> anyhow::Result<serde_json::Value> {
        let mut snapshot = snapshot(stores, s).await?;
        snapshot
            .as_object_mut()
            .context("a snapshot is an object")?
            .remove("job_plan_links");
        Ok(snapshot)
    }

    /// The target holds a second, unrelated set whose claims collide with the
    /// source's, so each claim goes back to the source's holder.
    #[tokio::test]
    async fn a_mirror_copy_makes_the_target_hold_the_source_records() -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let source = seed(&from).await?;
        let stale = seed(&to).await?;

        let merged = copy_records(&from, &to, CopyMode::Merge, true).await?;
        assert_eq!(merged.deleted(), 0, "a merge deletes nothing");
        assert!(merged.conflicts() > 0, "the claims collide: {merged:#?}");

        let first = copy_records(&from, &to, CopyMode::Mirror, false).await?;
        assert_eq!(first.conflicts(), 0, "{first:#?}");
        assert_eq!(first.unreadable(), 0, "{first:#?}");
        assert!(first.deleted() > 0, "{first:#?}");
        assert_eq!(
            mirrored(&to, &source).await?,
            mirrored(&from, &source).await?
        );

        assert_eq!(to.users.get_user(&stale.user.user_id).await?, None);
        assert!(
            to.users
                .list_user_keys(&stale.user.user_id)
                .await?
                .is_empty()
        );
        assert_eq!(to.orgs.get_membership(&stale.user.user_id).await?, None);
        assert_eq!(to.orgs.get_org(&stale.org.org_id).await?, None);
        assert_eq!(
            to.catalog
                .get_org_benchmark(&stale.org.org_id, &stale.benchmark_id)
                .await?,
            None
        );
        assert_eq!(to.campaigns.get_campaign(&stale.campaign_id).await?, None);
        assert!(!to.plans.has_cancel_marker(&stale.plan_id).await?);
        assert_eq!(to.plans.get_plan_source(&stale.orphan_plan_id).await?, None);
        assert_eq!(
            to.clients
                .get_client_preauth_key(&stale.live_client_key)
                .await?,
            None
        );
        assert!(
            to.plans.get_plan(&stale.plan_id).await?.is_some(),
            "no store method deletes a manifest, and the source never does"
        );

        let second = copy_records(&from, &to, CopyMode::Mirror, false).await?;
        assert_eq!(second.written(), 0, "{second:#?}");
        assert_eq!(second.deleted(), 0, "{second:#?}");
        assert_eq!(second.conflicts(), 0, "{second:#?}");
        Ok(())
    }

    /// A merge starts from an empty target, a mirror from an unrelated set.
    #[rstest::rstest]
    #[case::merge(CopyMode::Merge)]
    #[case::mirror(CopyMode::Mirror)]
    #[tokio::test]
    async fn a_dry_run_changes_nothing_and_predicts_the_copy(
        #[case] mode: CopyMode,
    ) -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (to_dir, to) = stores()?;
        let source = seed(&from).await?;
        let stale = match mode {
            CopyMode::Merge => None,
            CopyMode::Mirror => Some(seed(&to).await?),
        };
        // The state of the target that a dry run keeps: the files of an empty
        // target, or the records of the stale seed.
        let target = async || -> anyhow::Result<serde_json::Value> {
            match &stale {
                None => Ok(serde_json::to_value(files_under(to_dir.path())?)?),
                Some(stale) => snapshot(&to, stale).await,
            }
        };
        // The records that the copy makes equal on the two sides.
        let copied = async |stores: &Stores| match mode {
            CopyMode::Merge => snapshot(stores, &source).await,
            CopyMode::Mirror => mirrored(stores, &source).await,
        };
        let before = target().await?;

        let dry = copy_records(&from, &to, mode, true).await?;
        assert!(dry.written() > 0, "{dry:#?}");
        match mode {
            CopyMode::Merge => assert_eq!(before, serde_json::json!([]), "no file on the target"),
            CopyMode::Mirror => assert!(dry.deleted() > 0, "{dry:#?}"),
        }
        assert_eq!(target().await?, before);

        let real = copy_records(&from, &to, mode, false).await?;
        assert_eq!(
            CopyReport {
                dry_run: false,
                ..dry
            },
            real
        );
        assert_eq!(copied(&to).await?, copied(&from).await?);
        Ok(())
    }

    /// Write a record that does not parse to `<keyspace_dir>/<id>.json` in the
    /// directory of the source.
    fn corrupt_source(
        dir: &tempfile::TempDir,
        keyspace_dir: &str,
        id: impl Display,
    ) -> anyhow::Result<()> {
        let path = dir.path().join(keyspace_dir).join(format!("{id}.json"));
        std::fs::write(path, b"{ not json")?;
        Ok(())
    }

    #[tokio::test]
    async fn a_mirror_copy_keeps_a_record_the_source_cannot_read() -> anyhow::Result<()> {
        let (from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;
        copy_records(&from, &to, CopyMode::Merge, false).await?;
        corrupt_source(&from_dir, "campaigns", &seeded.campaign_id)?;

        let report = copy_records(&from, &to, CopyMode::Mirror, false).await?;
        let campaigns = keyspace(&report, "campaigns")?;
        assert_eq!(campaigns.deleted, 0, "{campaigns:#?}");
        assert!(!campaigns.unreadable.is_empty(), "{campaigns:#?}");
        assert!(
            to.campaigns
                .get_campaign(&seeded.campaign_id)
                .await?
                .is_some()
        );
        Ok(())
    }

    /// A store written before the refusal can hold a source naming `auth_token`;
    /// it is skipped and reported, and the target holds no token.
    #[tokio::test]
    async fn a_source_that_names_a_token_is_skipped() -> anyhow::Result<()> {
        let (from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;
        let gated = PlanId::from_uuid(uuid::Uuid::now_v7());
        from.plans
            .put_plan(&sample_manifest(gated.clone(), PlanStatus::Active, true)?)
            .await?;
        std::fs::write(
            from_dir
                .path()
                .join("plan-sources")
                .join(format!("{gated}.toml")),
            "# auth_token = \"hf_SECRET\"\nname = \"gated\"\n",
        )?;

        let report = copy_records(&from, &to, CopyMode::Merge, false).await?;
        let sources = keyspace(&report, "plan-sources")?;
        assert!(
            sources
                .skipped
                .iter()
                .any(|note| note.id.contains(gated.as_str()) && note.reason == NAMES_A_TOKEN),
            "{sources:#?}"
        );
        assert!(to.plans.get_plan_source(&gated).await?.is_none());
        assert!(to.plans.get_plan_source(&seeded.plan_id).await?.is_some());
        Ok(())
    }

    #[tokio::test]
    async fn an_unreadable_manifest_is_reported_and_not_copied() -> anyhow::Result<()> {
        let (from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let seeded = seed(&from).await?;
        let broken = PlanId::from_uuid(uuid::Uuid::now_v7());
        corrupt_source(&from_dir, "plans", &broken)?;

        let report = copy_records(&from, &to, CopyMode::Merge, false).await?;
        let plans = keyspace(&report, "plans")?;
        assert_eq!(plans.unreadable.len(), 1, "{plans:#?}");
        assert!(
            plans
                .unreadable
                .iter()
                .all(|note| note.id.contains(broken.as_str()))
        );
        assert_eq!(plans.read, 1, "the readable manifest still copies");
        assert!(to.plans.get_plan(&broken).await?.is_none());
        assert!(to.plans.get_plan(&seeded.plan_id).await?.is_some());
        Ok(())
    }

    /// End to end: one mirror run writes the key and clears the target's spent
    /// marker, and a second run changes nothing.
    #[tokio::test]
    async fn a_mirror_restores_a_key_spent_only_on_the_target() -> anyhow::Result<()> {
        let (_from_dir, from) = stores()?;
        let (_to_dir, to) = stores()?;
        let minted =
            client_preauth::mint(client_params(ClientPreauthUsage::SingleUse), Utc::now())?;
        from.clients.put_client_preauth_key(&minted.key).await?;
        let key_id = spend(&to, &minted).await?;

        let first = copy_records(&from, &to, CopyMode::Mirror, false).await?;
        assert_eq!(first.conflicts(), 0, "{first:#?}");
        assert!(to.clients.get_client_preauth_key(&key_id).await?.is_some());
        assert!(
            !to.clients
                .list_spent_client_preauth_markers()
                .await?
                .contains(&key_id)
        );

        let second = copy_records(&from, &to, CopyMode::Mirror, false).await?;
        assert_eq!(second.written(), 0, "{second:#?}");
        assert_eq!(second.deleted(), 0, "{second:#?}");
        Ok(())
    }

    /// A merge leaves the target's spend alone; a mirror clears the marker and
    /// writes the key in the same run.
    #[rstest::rstest]
    // (mirror, written, skipped, cleared)
    #[case::a_merge_keeps_the_target_spend(false, 0, 1, 0)]
    #[case::a_mirror_restores_the_key(true, 1, 0, 1)]
    fn a_key_spent_only_on_the_target(
        #[case] mirror: bool,
        #[case] written: usize,
        #[case] skipped: usize,
        #[case] cleared: usize,
    ) -> anyhow::Result<()> {
        // A spend consumes the key, so the target holds only the marker.
        let preauth = Preauth {
            mirror,
            records: vec![("k1".to_owned(), 1)],
            spent: BTreeSet::new(),
            target: BTreeMap::new(),
            target_spent: BTreeSet::from(["k1".to_owned()]),
        };
        let mut keys = KeyspaceReport::new("keys");
        let mut markers = KeyspaceReport::new("markers");
        let decided = preauth.decide(|k: &(String, u8)| &k.0, &mut keys, &mut markers)?;
        assert_eq!(decided.writes.len(), written, "{keys:#?}");
        assert_eq!(keys.written, written, "{keys:#?}");
        assert_eq!(keys.skipped.len(), skipped, "{keys:#?}");
        assert_eq!(decided.cleared_markers.len(), cleared, "{markers:#?}");
        assert!(decided.revoked.is_empty());
        Ok(())
    }
}
