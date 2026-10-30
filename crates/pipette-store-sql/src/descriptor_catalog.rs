//! SQL records for named runtime and model descriptors.

use std::collections::HashMap;
use std::num::NonZeroUsize;

use anyhow::Context;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::{Connection, FromRow, PgConnection};

use pipette_plan_types::{
    CatalogName, CatalogScope, DescriptorEntry, DescriptorEntryState, DescriptorMember,
    DescriptorMemberDescriptor, DescriptorMemberKey, NonEmptyVec, OrgId, QuantizationLabel,
};
use pipette_store::{
    AppendOutcome, CatalogEntryKey, CatalogEntryLock, CreateEntryOutcome, DescriptorCatalogStore,
    DescriptorEntryFilter, DescriptorEntryPage, DigestHit, SetStateOutcome, TransitionOutcome,
    UnknownCursorError, UnreadableObjectError,
};

use crate::db::SqlDatabase;
use crate::retry::retry;

/// The members of a set of entries, grouped by entry in the order of precedence.
const MEMBERS_OF: &str = "SELECT m.scope, m.name_key, m.descriptor_type, m.descriptor, m.digest,
     m.alias_digest,
     m.flavor, m.format, m.quantization, m.added_at
     FROM descriptor_members m
     JOIN unnest($1::text[], $2::text[]) AS keys(scope, name_key) USING (scope, name_key)
     ORDER BY m.scope, m.name_key, m.position";

const IMPORTER_ENTRIES_LOCK_KEY: i64 = 0x6c6c616d61636370;

#[derive(Clone)]
pub(crate) struct SqlDescriptorCatalogStore {
    db: SqlDatabase,
}

struct SqlCatalogEntryLock(PgConnection);

#[async_trait]
impl CatalogEntryLock for SqlCatalogEntryLock {
    async fn release(self: Box<Self>) -> anyhow::Result<()> {
        let Self(mut connection) = *self;
        let unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
            .bind(IMPORTER_ENTRIES_LOCK_KEY)
            .fetch_one(&mut connection)
            .await?;
        anyhow::ensure!(unlocked, "catalog importer state lock was lost");
        connection.close().await?;
        Ok(())
    }
}

impl SqlDescriptorCatalogStore {
    pub(crate) fn new(db: SqlDatabase) -> Self {
        Self { db }
    }

    async fn apply_state(
        connection: &mut PgConnection,
        scope: &str,
        key: &str,
        state: DescriptorEntryState,
        writes: &(impl Fn(&str) -> bool + Sync),
    ) -> sqlx::Result<Option<String>> {
        let old: Option<(String,)> = sqlx::query_as(
            "SELECT state FROM descriptor_entries WHERE scope = $1 AND name_key = $2 FOR UPDATE",
        )
        .bind(scope)
        .bind(key)
        .fetch_optional(&mut *connection)
        .await?;
        if let Some((old,)) = &old
            && writes(old)
        {
            sqlx::query(
                "UPDATE descriptor_entries SET state = $3 WHERE scope = $1 AND name_key = $2",
            )
            .bind(scope)
            .bind(key)
            .bind(state.as_ref())
            .execute(&mut *connection)
            .await?;
        }
        Ok(old.map(|(old,)| old))
    }

    /// Lock the entry row, and write `state` if `writes` accepts the old state.
    /// The result is the old state, or `None` for a missing entry.
    async fn write_state(
        &self,
        what: &'static str,
        scope: &CatalogScope,
        name: &CatalogName,
        state: DescriptorEntryState,
        lock_importer: bool,
        writes: impl Fn(&str) -> bool + Send + Sync,
    ) -> anyhow::Result<Option<DescriptorEntryState>> {
        let key = name.key();
        let scope = scope.sql_key();
        let options = self.db.pool().connect_options();
        let old: Option<String> = retry(what, || {
            Box::pin(async {
                if lock_importer {
                    // Do not occupy the pool while waiting for the importer.
                    // Its own writes may need the only pooled connection.
                    let mut connection = PgConnection::connect_with(options.as_ref()).await?;
                    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ")
                        .execute(&mut connection)
                        .await?;
                    sqlx::query("SELECT pg_advisory_xact_lock($1)")
                        .bind(IMPORTER_ENTRIES_LOCK_KEY)
                        .execute(&mut connection)
                        .await?;
                    let old =
                        Self::apply_state(&mut connection, scope, &key, state, &writes).await?;
                    sqlx::query("COMMIT").execute(&mut connection).await?;
                    Ok(old)
                } else {
                    let mut tx = self.db.begin().await?;
                    let old = Self::apply_state(&mut tx, scope, &key, state, &writes).await?;
                    tx.commit().await?;
                    Ok(old)
                }
            })
        })
        .await?;
        Ok(old.map(|old| old.parse()).transpose()?)
    }

    async fn transition_state_inner(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
        lock_importer: bool,
    ) -> anyhow::Result<TransitionOutcome> {
        let old = self
            .write_state(
                "transition_descriptor_entry_state",
                scope,
                name,
                state,
                lock_importer,
                |old| old == expected.as_ref() && old != DescriptorEntryState::Tombstoned.as_ref(),
            )
            .await?;
        Ok(match old {
            None => TransitionOutcome::NoSuchEntry,
            Some(DescriptorEntryState::Tombstoned) => TransitionOutcome::Tombstoned,
            Some(old) if old != expected => TransitionOutcome::UnexpectedState(old),
            Some(_) => TransitionOutcome::Changed,
        })
    }
}

/// A `descriptor_entries` row before the store parses it.
#[derive(FromRow)]
struct EntryRow {
    scope: String,
    name_key: String,
    name: String,
    kind: String,
    state: String,
    descriptor_kind: String,
    descriptor_type: String,
    release_time: DateTime<Utc>,
    created_at: DateTime<Utc>,
    created_by: String,
}

/// A `descriptor_members` row before the store parses it.
#[derive(FromRow)]
struct MemberRow {
    scope: String,
    name_key: String,
    descriptor_type: String,
    descriptor: String,
    digest: String,
    alias_digest: Option<String>,
    flavor: Option<String>,
    format: Option<String>,
    quantization: Option<String>,
    added_at: DateTime<Utc>,
}

/// The key that a member row's columns record: a flavor for a runtime member,
/// or a format and a quantization for a model member.
fn member_key(
    flavor: Option<String>,
    format: Option<String>,
    quantization: Option<String>,
) -> anyhow::Result<DescriptorMemberKey> {
    match (flavor, format, quantization) {
        (flavor, None, None) => Ok(DescriptorMemberKey::Runtime { flavor }),
        (None, Some(format), Some(quantization)) => Ok(DescriptorMemberKey::Model {
            format: format.parse()?,
            quantization: QuantizationLabel::try_new(quantization)?,
        }),
        _ => anyhow::bail!("member row mixes a flavor with a format and a quantization"),
    }
}

impl MemberRow {
    fn parse(self) -> anyhow::Result<DescriptorMember> {
        let key = member_key(self.flavor, self.format, self.quantization)?;
        let descriptor = match key {
            DescriptorMemberKey::Runtime { .. } => {
                DescriptorMemberDescriptor::Runtime(serde_json::from_str(&self.descriptor)?)
            }
            DescriptorMemberKey::Model { .. } => {
                DescriptorMemberDescriptor::Model(serde_json::from_str(&self.descriptor)?)
            }
        };
        let member = DescriptorMember::verified(descriptor, &self.digest, &key, self.added_at)?
            .with_alias_digest(self.alias_digest);
        anyhow::ensure!(
            member.descriptor_type() == self.descriptor_type,
            "member descriptor type mismatch"
        );
        Ok(member)
    }
}

impl EntryRow {
    /// The entry that this row and its member rows, in position order, record.
    fn parse(self, members: Vec<MemberRow>) -> anyhow::Result<DescriptorEntry> {
        let members = NonEmptyVec::try_new(
            members
                .into_iter()
                .map(MemberRow::parse)
                .collect::<anyhow::Result<Vec<_>>>()?,
        )?;
        DescriptorEntry::validate_members(&members)?;
        let entry = DescriptorEntry {
            scope: CatalogScope::from_sql_key(&self.scope)?,
            name: CatalogName::try_new(self.name)?,
            kind: self.kind.parse()?,
            state: self.state.parse()?,
            release_time: self.release_time,
            created_at: self.created_at,
            created_by: self.created_by.parse()?,
            members,
        };
        anyhow::ensure!(entry.name.key() == self.name_key, "entry name key mismatch");
        DescriptorEntry::validate_identity(
            &entry.scope,
            entry.kind,
            &entry.created_by,
            entry.descriptor_kind(),
        )?;
        anyhow::ensure!(
            entry.descriptor_kind().as_ref() == self.descriptor_kind,
            "entry descriptor kind mismatch"
        );
        anyhow::ensure!(
            entry.members.head().descriptor_type() == self.descriptor_type,
            "entry descriptor type mismatch"
        );
        Ok(entry)
    }
}

/// The columns of a member to insert, encoded once before the transaction.
struct NewMember<'a> {
    offset: i32,
    descriptor_type: String,
    flavor: Option<&'a str>,
    format: Option<String>,
    quantization: Option<&'a str>,
    digest: &'a str,
    alias_digest: Option<&'a str>,
    descriptor: String,
    added_at: DateTime<Utc>,
}

fn new_members(members: &NonEmptyVec<DescriptorMember>) -> anyhow::Result<Vec<NewMember<'_>>> {
    members
        .iter()
        .enumerate()
        .map(|(offset, member)| {
            let model_key = member.model_key();
            Ok(NewMember {
                offset: i32::try_from(offset)?,
                descriptor_type: member.descriptor_type(),
                flavor: member.flavor(),
                format: model_key.map(|(format, _)| format.to_string()),
                quantization: model_key.map(|(_, quantization)| quantization.as_str()),
                digest: member.digest(),
                alias_digest: member.alias_digest(),
                descriptor: pipette_plan_types::canonical_json(&serde_json::to_value(
                    member.descriptor(),
                )?),
                added_at: member.added_at(),
            })
        })
        .collect()
}

/// Insert `members` into the entry `key`, the first at position `start`.
async fn insert_members(
    tx: &mut PgConnection,
    scope: &str,
    key: &str,
    start: i32,
    members: &[NewMember<'_>],
) -> Result<(), sqlx::Error> {
    for member in members {
        sqlx::query(
            "INSERT INTO descriptor_members (scope, name_key, position, descriptor_type, flavor,
             format, quantization, digest, alias_digest, descriptor, added_at)
             VALUES ($1, $2, $3 + $4, $5, $6, $7, $8, $9, $10, $11, $12)",
        )
        .bind(scope)
        .bind(key)
        .bind(start)
        .bind(member.offset)
        .bind(&member.descriptor_type)
        .bind(member.flavor)
        .bind(&member.format)
        .bind(member.quantization)
        .bind(member.digest)
        .bind(member.alias_digest)
        .bind(&member.descriptor)
        .bind(member.added_at)
        .execute(&mut *tx)
        .await?;
    }
    Ok(())
}

/// The refusal of `new` after the `existing` member rows, or `None` if every
/// member can share the entry.
fn append_refusal(
    existing: Vec<MemberRow>,
    new: &NonEmptyVec<DescriptorMember>,
) -> anyhow::Result<Option<AppendOutcome>> {
    let mut members = existing
        .into_iter()
        .map(MemberRow::parse)
        .collect::<anyhow::Result<Vec<_>>>()?;
    members.extend(new.iter().cloned());
    match DescriptorEntry::validate_members(&NonEmptyVec::try_new(members)?) {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(AppendOutcome::refusing(error)?)),
    }
}

#[async_trait]
impl DescriptorCatalogStore for SqlDescriptorCatalogStore {
    async fn lock_importer_entries(&self) -> anyhow::Result<Box<dyn CatalogEntryLock>> {
        let options = self.db.pool().connect_options();
        let mut connection = PgConnection::connect_with(options.as_ref()).await?;
        sqlx::query("SELECT pg_advisory_lock($1)")
            .bind(IMPORTER_ENTRIES_LOCK_KEY)
            .execute(&mut connection)
            .await?;
        Ok(Box::new(SqlCatalogEntryLock(connection)))
    }

    async fn create_entry(&self, entry: &DescriptorEntry) -> anyhow::Result<CreateEntryOutcome> {
        anyhow::ensure!(
            entry.state != DescriptorEntryState::Tombstoned,
            "cannot create a tombstoned entry"
        );
        DescriptorEntry::validate_members(&entry.members)?;
        DescriptorEntry::validate_identity(
            &entry.scope,
            entry.kind,
            &entry.created_by,
            entry.descriptor_kind(),
        )?;
        let scope = entry.scope.sql_key();
        let key = entry.name.key();
        let descriptor_kind = entry.descriptor_kind();
        let descriptor_type = entry.members.head().descriptor_type();
        let created_by = entry.created_by.to_string();
        let members = new_members(&entry.members)?;
        let holder: Option<(String, String)> = retry("create_descriptor_entry", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let inserted = sqlx::query(
                    "INSERT INTO descriptor_entries (scope, name_key, name, kind, state, descriptor_kind,
                     descriptor_type, release_time, created_at, created_by)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                     ON CONFLICT (scope, name_key) DO NOTHING",
                )
                .bind(scope)
                .bind(&key)
                .bind(entry.name.as_str())
                .bind(entry.kind.as_ref())
                .bind(entry.state.as_ref())
                .bind(descriptor_kind.as_ref())
                .bind(&descriptor_type)
                .bind(entry.release_time)
                .bind(entry.created_at)
                .bind(&created_by)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                let holder = if inserted == 0 {
                    Some(
                        sqlx::query_as(
                            "SELECT name, state FROM descriptor_entries WHERE scope = $1 AND name_key = $2",
                        )
                        .bind(scope)
                        .bind(&key)
                        .fetch_one(&mut *tx)
                        .await?,
                    )
                } else {
                    insert_members(&mut tx, scope, &key, 0, &members).await?;
                    None
                };
                tx.commit().await?;
                Ok(holder)
            })
        })
        .await?;
        Ok(match holder {
            None => CreateEntryOutcome::Created,
            Some((name, state)) => CreateEntryOutcome::NameTaken {
                name: CatalogName::try_new(name)?,
                state: state.parse()?,
            },
        })
    }

    async fn append_members(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        members: NonEmptyVec<DescriptorMember>,
    ) -> anyhow::Result<AppendOutcome> {
        if let Err(error) = DescriptorEntry::validate_members(&members) {
            return Ok(AppendOutcome::refusing(error)?);
        }
        let keys = [name.key()];
        let scopes = [scope.sql_key()];
        let rows = new_members(&members)?;
        retry("append_descriptor_members", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let row: Option<EntryRow> = sqlx::query_as(
                    "UPDATE descriptor_entries SET state = state WHERE scope = $1 AND name_key = $2
                     RETURNING scope, name_key, name, kind, state, descriptor_kind,
                     descriptor_type, release_time, created_at, created_by",
                )
                .bind(scopes[0])
                .bind(&keys[0])
                .fetch_optional(&mut *tx)
                .await?;
                let Some(row) = row else {
                    return Ok(Ok(AppendOutcome::NoSuchEntry));
                };
                if row.state == DescriptorEntryState::Tombstoned.as_ref() {
                    return Ok(Ok(AppendOutcome::Tombstoned));
                }
                if row.descriptor_kind != members.head().descriptor_kind().as_ref() {
                    return Ok(Ok(AppendOutcome::DescriptorKindMismatch));
                }
                if row.descriptor_kind == "runtime"
                    && row.descriptor_type != members.head().descriptor_type()
                {
                    return Ok(Ok(AppendOutcome::RuntimeTypeMismatch));
                }
                // The row lock above serializes appends, so this read sees every member.
                let existing: Vec<MemberRow> = sqlx::query_as(MEMBERS_OF)
                    .bind(&scopes[..])
                    .bind(&keys[..])
                    .fetch_all(&mut *tx)
                    .await?;
                match append_refusal(existing, &members) {
                    Ok(None) => {}
                    Ok(Some(outcome)) => return Ok(Ok(outcome)),
                    Err(error) => return Ok(Err(error)),
                }
                let (start,): (i32,) = sqlx::query_as(
                    "SELECT COALESCE(MAX(position) + 1, 0) FROM descriptor_members
                     WHERE scope = $1 AND name_key = $2",
                )
                .bind(scopes[0])
                .bind(&keys[0])
                .fetch_one(&mut *tx)
                .await?;
                insert_members(&mut tx, scopes[0], &keys[0], start, &rows).await?;
                let committed_members: Vec<MemberRow> = sqlx::query_as(MEMBERS_OF)
                    .bind(&scopes[..])
                    .bind(&keys[..])
                    .fetch_all(&mut *tx)
                    .await?;
                let entry = match row.parse(committed_members) {
                    Ok(entry) => entry,
                    Err(error) => return Ok(Err(error)),
                };
                tx.commit().await?;
                Ok(Ok(AppendOutcome::Appended(entry)))
            })
        })
        .await?
    }

    async fn set_state(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        state: DescriptorEntryState,
    ) -> anyhow::Result<SetStateOutcome> {
        let old = self
            .write_state(
                "set_descriptor_entry_state",
                scope,
                name,
                state,
                true,
                |old| old != DescriptorEntryState::Tombstoned.as_ref() && old != state.as_ref(),
            )
            .await?;
        Ok(match old {
            None => SetStateOutcome::NoSuchEntry,
            Some(DescriptorEntryState::Tombstoned) => SetStateOutcome::Tombstoned,
            Some(from) if from == state => SetStateOutcome::Unchanged,
            Some(from) => SetStateOutcome::Changed { from },
        })
    }

    async fn transition_state(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome> {
        self.transition_state_inner(scope, name, expected, state, true)
            .await
    }

    async fn transition_state_under_importer_lock(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
        expected: DescriptorEntryState,
        state: DescriptorEntryState,
    ) -> anyhow::Result<TransitionOutcome> {
        self.transition_state_inner(scope, name, expected, state, false)
            .await
    }

    async fn get_entry(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
    ) -> anyhow::Result<Option<DescriptorEntry>> {
        let keys = [name.key()];
        let scopes = [scope.sql_key()];
        let (entry, members) = retry("get_descriptor_entry", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let entry: Option<EntryRow> = sqlx::query_as(
                    "SELECT scope, name_key, name, kind, state, descriptor_kind, descriptor_type,
                     release_time, created_at, created_by
                     FROM descriptor_entries WHERE scope = $1 AND name_key = $2",
                )
                .bind(scopes[0])
                .bind(&keys[0])
                .fetch_optional(&mut *tx)
                .await?;
                let members: Vec<MemberRow> = match entry {
                    Some(_) => {
                        sqlx::query_as(MEMBERS_OF)
                            .bind(&scopes[..])
                            .bind(&keys[..])
                            .fetch_all(&mut *tx)
                            .await?
                    }
                    None => Vec::new(),
                };
                tx.commit().await?;
                Ok((entry, members))
            })
        })
        .await?;
        entry.map(|row| row.parse(members)).transpose()
    }

    async fn list_entries(
        &self,
        filter: &DescriptorEntryFilter,
        after: Option<&CatalogEntryKey>,
        limit: NonZeroUsize,
    ) -> anyhow::Result<DescriptorEntryPage> {
        if filter.scopes.is_empty() {
            return Ok(DescriptorEntryPage::default());
        }
        // A release time never changes, so the cursor can come from its own snapshot.
        let cursor: Option<(DateTime<Utc>, String, String)> = match after {
            Some(after) => {
                let key = after.name.key();
                let scope = after.scope.sql_key().to_owned();
                let (release_time,): (DateTime<Utc>,) = self
                    .db
                    .fetch_optional("find_descriptor_catalog_cursor", || {
                        sqlx::query_as(
                            "SELECT release_time FROM descriptor_entries WHERE scope = $1 AND name_key = $2",
                        )
                        .bind(&scope)
                        .bind(&key)
                    })
                    .await?
                    .ok_or_else(|| UnknownCursorError {
                        cursor: after.clone(),
                    })?;
                Some((release_time, scope, key))
            }
            None => None,
        };
        let kinds: Vec<String> = filter.kinds.iter().map(ToString::to_string).collect();
        let scopes: Vec<&str> = filter.scopes.iter().map(CatalogScope::sql_key).collect();
        let states: Vec<String> = filter.states.iter().map(ToString::to_string).collect();
        let descriptor_kind = filter.descriptor_kind.map(|kind| kind.as_ref().to_owned());
        let runtime_types: Vec<String> = filter
            .runtime_types
            .iter()
            .map(ToString::to_string)
            .collect();
        let model_types: Vec<String> = filter.model_types.iter().map(ToString::to_string).collect();
        // Rust folds the prefix with ASCII rules, as it folds the key, so the
        // database locale has no effect.
        let name_prefix = filter.name_prefix.as_deref().map(str::to_ascii_lowercase);
        let fetch = i64::try_from(limit.get())?
            .checked_add(1)
            .context("the page limit for the descriptor catalog is too large")?;
        // One snapshot holds the page's entries and their members together.
        let (entries, members) = retry("list_descriptor_entries", || {
            Box::pin(async {
                let mut tx = self.db.begin().await?;
                let entries: Vec<EntryRow> = sqlx::query_as(
                    "SELECT scope, name_key, name, kind, state, descriptor_kind, descriptor_type,
             release_time, created_at, created_by FROM descriptor_entries e WHERE
             scope = ANY($1) AND
             (cardinality($2::text[]) = 0 OR kind = ANY($2)) AND
             (cardinality($3::text[]) = 0 OR state = ANY($3)) AND
             ($4::text IS NULL OR descriptor_kind = $4) AND
             ((cardinality($5::text[]) = 0 AND cardinality($6::text[]) = 0) OR
                 (descriptor_kind = 'runtime' AND descriptor_type = ANY($5)) OR
                 (descriptor_kind = 'model' AND EXISTS (
                     SELECT 1 FROM descriptor_members m
                     WHERE (m.scope, m.name_key) = (e.scope, e.name_key)
                         AND m.descriptor_type = ANY($6)))) AND
             ($7::text IS NULL OR starts_with(name_key, $7)) AND
             ($8::timestamptz IS NULL OR release_time < $8 OR
                 (release_time = $8 AND (scope, name_key) > ($9, $10)))
             ORDER BY release_time DESC, scope, name_key LIMIT $11",
                )
                .bind(&scopes)
                .bind(&kinds)
                .bind(&states)
                .bind(&descriptor_kind)
                .bind(&runtime_types)
                .bind(&model_types)
                .bind(&name_prefix)
                .bind(cursor.as_ref().map(|c| c.0))
                .bind(cursor.as_ref().map(|c| &c.1))
                .bind(cursor.as_ref().map(|c| &c.2))
                .bind(fetch)
                .fetch_all(&mut *tx)
                .await?;
                let keys: Vec<&str> = entries
                    .iter()
                    .take(limit.get())
                    .map(|row| row.name_key.as_str())
                    .collect();
                let member_scopes: Vec<&str> = entries
                    .iter()
                    .take(limit.get())
                    .map(|row| row.scope.as_str())
                    .collect();
                let members: Vec<MemberRow> = sqlx::query_as(MEMBERS_OF)
                    .bind(&member_scopes)
                    .bind(&keys)
                    .fetch_all(&mut *tx)
                    .await?;
                tx.commit().await?;
                Ok((entries, members))
            })
        })
        .await?;
        let has_more = entries.len() > limit.get();
        let rows: Vec<EntryRow> = entries.into_iter().take(limit.get()).collect();
        let resume_after = match rows.last() {
            Some(row) if has_more => Some(CatalogEntryKey {
                scope: CatalogScope::from_sql_key(&row.scope)?,
                name: CatalogName::try_new(row.name_key.clone())?,
            }),
            _ => None,
        };
        let mut members = members.into_iter().fold(
            HashMap::<(String, String), Vec<MemberRow>>::new(),
            |mut groups, row| {
                groups
                    .entry((row.scope.clone(), row.name_key.clone()))
                    .or_default()
                    .push(row);
                groups
            },
        );
        Ok(rows.into_iter().fold(
            DescriptorEntryPage {
                resume_after,
                ..Default::default()
            },
            |mut page, row| {
                let key = (row.scope.clone(), row.name_key.clone());
                match row.parse(members.remove(&key).unwrap_or_default()) {
                    Ok(entry) => page.items.push(entry),
                    Err(error) => page.unreadable.push(UnreadableObjectError {
                        key: if key.0.is_empty() {
                            format!("descriptor-catalog/{}", key.1)
                        } else {
                            format!("descriptor-catalog/{}/{}", key.0, key.1)
                        },
                        reason: format!("{error:#}"),
                    }),
                }
                page
            },
        ))
    }

    async fn find_by_digests(
        &self,
        digests: &[String],
    ) -> anyhow::Result<HashMap<String, Vec<DigestHit>>> {
        #[derive(FromRow)]
        struct HitRow {
            digest: String,
            scope: String,
            name: String,
            kind: String,
            state: String,
            flavor: Option<String>,
            format: Option<String>,
            quantization: Option<String>,
        }
        let rows: Vec<HitRow> = self
            .db
            .fetch_all("find_descriptor_entries_by_digest", || {
                sqlx::query_as(
                    "SELECT wanted.digest, e.scope, e.name, e.kind, e.state, m.flavor, m.format,
                     m.quantization
                     FROM unnest($1::text[]) AS wanted(digest)
                     JOIN descriptor_members m ON wanted.digest = m.digest
                         OR wanted.digest = m.alias_digest
                     JOIN descriptor_entries e
                         ON (e.scope, e.name_key) = (m.scope, m.name_key)
                     ",
                )
                .bind(digests)
            })
            .await?;
        rows.into_iter()
            .try_fold(HashMap::<String, Vec<DigestHit>>::new(), |mut hits, row| {
                hits.entry(row.digest).or_default().push(DigestHit {
                    scope: CatalogScope::from_sql_key(&row.scope)?,
                    name: CatalogName::try_new(row.name)?,
                    kind: row.kind.parse()?,
                    state: row.state.parse()?,
                    member_key: member_key(row.flavor, row.format, row.quantization)?,
                });
                Ok(hits)
            })
    }

    async fn live_entries_in_org(&self, org_id: &OrgId) -> anyhow::Result<Vec<CatalogName>> {
        let names: Vec<(String,)> = self.db.fetch_all("live_catalog_entries_in_org", || {
            sqlx::query_as("SELECT name FROM descriptor_entries WHERE scope = $1 AND state <> 'tombstoned' ORDER BY name_key")
                .bind(org_id.as_str())
        }).await?;
        names
            .into_iter()
            .map(|(name,)| CatalogName::try_new(name).map_err(Into::into))
            .collect()
    }

    async fn delete_tombstones_in_org(&self, org_id: &OrgId) -> anyhow::Result<()> {
        self.db
            .execute("delete_catalog_tombstones_in_org", || {
                sqlx::query(
                    "DELETE FROM descriptor_entries WHERE scope = $1 AND state = 'tombstoned'",
                )
                .bind(org_id.as_str())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[tokio::test]
async fn importer_lock_serializes_operator_state_changes_with_one_pooled_connection()
-> anyhow::Result<()> {
    use std::time::Duration;

    use crate::testing::test_db_or_skip;

    let test_db = test_db_or_skip!();
    let options = test_db.db().pool().connect_options();
    let db = SqlDatabase::from_options(options.as_ref().clone(), 1);
    let store = SqlDescriptorCatalogStore::new(db);
    let importer_lock = store.lock_importer_entries().await?;
    let name = CatalogName::try_new("missing")?;
    let mut operator =
        Box::pin(store.set_state(&CatalogScope::Global, &name, DescriptorEntryState::Hidden));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut operator)
            .await
            .is_err()
    );
    importer_lock.release().await?;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), operator).await??,
        SetStateOutcome::NoSuchEntry
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_db_or_skip;

    /// Create one stable entry with a single `b1` member, as the setup of a test.
    async fn seed(
        store: &SqlDescriptorCatalogStore,
        name: &str,
        release_second: i64,
        version: &str,
    ) -> anyhow::Result<DescriptorEntry> {
        let entry = pipette_store_testkit::a_descriptor_entry(
            name,
            pipette_plan_types::DescriptorEntryKind::Stable,
            release_second,
            pipette_store_testkit::a_member(version, "linux-x64-rocm")?,
        )?;
        assert_eq!(
            store.create_entry(&entry).await?,
            CreateEntryOutcome::Created
        );
        Ok(entry)
    }

    #[tokio::test]
    async fn an_unreadable_member_is_reported() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlDescriptorCatalogStore::new(test_db.db().clone());
        seed(&store, "broken-1", 100, "b1").await?;
        sqlx::query(
            "UPDATE descriptor_members SET descriptor = 'not json' WHERE name_key = 'broken-1'",
        )
        .execute(test_db.db().pool())
        .await?;
        let good = seed(&store, "good-1", 50, "b2").await?;
        let filter = DescriptorEntryFilter {
            scopes: [CatalogScope::Global].into(),
            ..Default::default()
        };
        let page = store.list_entries(&filter, None, NonZeroUsize::MIN).await?;
        assert!(page.items.is_empty());
        assert_eq!(page.unreadable.len(), 1);
        assert_eq!(
            page.unreadable.first().context("unreadable row")?.key,
            "descriptor-catalog/broken-1"
        );
        let next = store
            .list_entries(&filter, page.resume_after.as_ref(), NonZeroUsize::MIN)
            .await?;
        assert!(next.unreadable.is_empty());
        assert_eq!(next.items.first().context("next entry")?.name, good.name);
        assert!(next.resume_after.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn append_checks_the_entry_type_when_its_members_are_missing() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlDescriptorCatalogStore::new(test_db.db().clone());
        let entry = seed(&store, "missing-1", 100, "b1").await?;
        sqlx::query("DELETE FROM descriptor_members WHERE name_key = 'missing-1'")
            .execute(test_db.db().pool())
            .await?;

        let wrong = DescriptorMember::new(
            pipette_plan_types::Runtime::AppleFoundation(Default::default()),
            Utc::now(),
        )?;
        assert_eq!(
            store
                .append_members(
                    &CatalogScope::Global,
                    &entry.name,
                    NonEmptyVec::try_new(vec![wrong])?,
                )
                .await?,
            AppendOutcome::RuntimeTypeMismatch
        );

        let replacement = pipette_store_testkit::a_member("b2", "linux-x64-rocm")?;
        let outcome = store
            .append_members(
                &CatalogScope::Global,
                &entry.name,
                NonEmptyVec::try_new(vec![replacement.clone()])?,
            )
            .await?;
        assert!(
            matches!(&outcome, AppendOutcome::Appended(stored) if stored.members.as_ref() == [replacement.clone()]),
            "{outcome:?}"
        );
        assert_eq!(
            store
                .get_entry(&CatalogScope::Global, &entry.name)
                .await?
                .context("repaired entry")?
                .members
                .as_ref(),
            [replacement]
        );
        Ok(())
    }

    #[tokio::test]
    async fn racing_appends_keep_every_position() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlDescriptorCatalogStore::new(test_db.db().clone());
        let entry = seed(&store, "race-1", 100, "b1").await?;
        let scope = CatalogScope::Global;
        let outcomes = crate::testing::race(|n| {
            let store = store.clone();
            let name = entry.name.clone();
            let scope = scope.clone();
            async move {
                let member = pipette_store_testkit::a_member(
                    &format!("b{}", n + 2),
                    &format!("custom-{n}"),
                )?;
                store
                    .append_members(&scope, &name, NonEmptyVec::try_new(vec![member])?)
                    .await
            }
        })
        .await?;
        let mut sizes = outcomes
            .iter()
            .filter_map(|outcome| {
                if let AppendOutcome::Appended(stored) = outcome {
                    Some(stored.members.len())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(sizes.len(), crate::testing::RACERS, "{outcomes:?}");
        sizes.sort_unstable();
        assert_eq!(sizes, (2..=crate::testing::RACERS + 1).collect::<Vec<_>>());
        let current = store
            .get_entry(&scope, &entry.name)
            .await?
            .context("raced entry")?;
        assert_eq!(current.members.len(), crate::testing::RACERS + 1);
        let positions: Vec<i32> = sqlx::query_scalar(
            "SELECT position FROM descriptor_members WHERE name_key = 'race-1' ORDER BY position",
        )
        .fetch_all(test_db.db().pool())
        .await?;
        assert_eq!(
            positions,
            (0..i32::try_from(positions.len())?).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn meets_the_shared_contract() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlDescriptorCatalogStore::new(test_db.db().clone());
        pipette_store_testkit::assert_descriptor_catalog_store_contract(&store).await
    }

    #[tokio::test]
    async fn meets_the_model_contract() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let store = SqlDescriptorCatalogStore::new(test_db.db().clone());
        pipette_store_testkit::assert_descriptor_catalog_model_contract(&store).await
    }

    /// Write a global runtime entry with one member in the columns that
    /// migrations 15 and 16 have.
    async fn insert_legacy(db: &crate::db::SqlDatabase) -> anyhow::Result<DescriptorEntry> {
        let legacy = pipette_store_testkit::a_descriptor_entry(
            "Same-1",
            pipette_plan_types::DescriptorEntryKind::Stable,
            100,
            pipette_store_testkit::a_member("b1", "macos-arm64")?,
        )?;
        let member = legacy.members.head();
        sqlx::query("INSERT INTO descriptor_entries (name_key, name, kind, state, runtime_type, release_time, created_at, created_by) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
            .bind(legacy.name.key()).bind(legacy.name.as_str()).bind(legacy.kind.to_string())
            .bind(legacy.state.to_string()).bind(member.descriptor_type())
            .bind(legacy.release_time).bind(legacy.created_at).bind(legacy.created_by.to_string())
            .execute(db.pool()).await?;
        let descriptor =
            pipette_plan_types::canonical_json(&serde_json::to_value(member.descriptor())?);
        sqlx::query("INSERT INTO descriptor_members (name_key, position, flavor, digest, descriptor, added_at) VALUES ($1,0,$2,$3,$4,$5)")
            .bind(legacy.name.key()).bind(member.flavor()).bind(member.digest())
            .bind(descriptor).bind(member.added_at()).execute(db.pool()).await?;
        Ok(legacy)
    }

    async fn count_rows(db: &crate::db::SqlDatabase) -> anyhow::Result<(i64, i64)> {
        let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM descriptor_entries")
            .fetch_one(db.pool())
            .await?;
        let members: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM descriptor_members")
            .fetch_one(db.pool())
            .await?;
        Ok((entries, members))
    }

    #[tokio::test]
    async fn migration_keeps_global_rows_and_down_removes_org_rows() -> anyhow::Result<()> {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(15), false).await?;
        let legacy = insert_legacy(db).await?;
        crate::migrate::migrate(db, None, false).await?;
        let store = SqlDescriptorCatalogStore::new(db.clone());
        assert_eq!(
            store.get_entry(&CatalogScope::Global, &legacy.name).await?,
            Some(legacy.clone())
        );
        let scope = CatalogScope::Org(OrgId::try_new("org_a")?);
        let mut org_entry = legacy.clone();
        org_entry.scope = scope.clone();
        org_entry.kind = pipette_plan_types::DescriptorEntryKind::HandMade;
        assert_eq!(
            store.create_entry(&org_entry).await?,
            CreateEntryOutcome::Created
        );
        crate::migrate::migrate(db, Some(15), false).await?;
        assert_eq!(
            count_rows(db).await?,
            (1, 1),
            "org members are deleted with their entry"
        );
        crate::migrate::migrate(db, None, false).await?;
        assert_eq!(
            store.get_entry(&CatalogScope::Global, &legacy.name).await?,
            Some(legacy)
        );
        assert!(store.get_entry(&scope, &org_entry.name).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn migration_types_runtime_members_and_down_removes_model_entries() -> anyhow::Result<()>
    {
        let test_db = test_db_or_skip!();
        let db = test_db.db();
        crate::migrate::migrate(db, Some(16), false).await?;
        let legacy = insert_legacy(db).await?;
        crate::migrate::migrate(db, Some(17), false).await?;
        let types: Vec<(String, String)> = sqlx::query_as(
            "SELECT e.descriptor_kind, m.descriptor_type FROM descriptor_entries e
             JOIN descriptor_members m USING (scope, name_key)",
        )
        .fetch_all(db.pool())
        .await?;
        assert_eq!(
            types,
            [("runtime".to_owned(), "llamacpp_cli_stock_tools".to_owned())]
        );
        // The store reads the current schema; the assertion above checked the
        // migration at 17 before later columns were added.
        crate::migrate::migrate(db, None, false).await?;
        let store = SqlDescriptorCatalogStore::new(db.clone());
        assert_eq!(
            store.get_entry(&CatalogScope::Global, &legacy.name).await?,
            Some(legacy.clone())
        );
        let model = pipette_store_testkit::a_descriptor_entry(
            "AcmeLM-3B",
            pipette_plan_types::DescriptorEntryKind::HandMade,
            200,
            pipette_store_testkit::a_model_member("mlx", "AcmeLM-3B-MLX-4bit", "4bit")?,
        )?;
        assert_eq!(
            store.create_entry(&model).await?,
            CreateEntryOutcome::Created
        );
        crate::migrate::migrate(db, Some(16), false).await?;
        assert_eq!(
            count_rows(db).await?,
            (1, 1),
            "model members are deleted with their entry"
        );
        crate::migrate::migrate(db, None, false).await?;
        assert_eq!(
            store.get_entry(&CatalogScope::Global, &legacy.name).await?,
            Some(legacy)
        );
        assert!(
            store
                .get_entry(&CatalogScope::Global, &model.name)
                .await?
                .is_none()
        );
        Ok(())
    }
}
