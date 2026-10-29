//! This module selects the descriptor catalog entries that one reader can read.
//! It gives the name that each entry shows to that reader. It also looks up the
//! names that the reader writes, and the names of the digests that a row shows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use pipette_plan_types::{
    CatalogName, CatalogQualifier, CatalogScope, DescriptorDisplay, DescriptorEntry,
    DescriptorEntryKind, DescriptorEntryState, DescriptorMemberKey, MemberSelector, OrgId, OrgSlug,
    QualifiedCatalogName,
};
use pipette_store::{DescriptorCatalogStore, DigestHit, OrgStore};

/// The catalog scopes that one reader can read. Every reader reads the global
/// scope. A member of an organization reads the entries of that organization
/// and of no other. A holder of `edit_any_catalog` reads the entries of every
/// organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogReadScope {
    /// A reader that belongs to no organization reads the global scope only.
    Unaffiliated,
    /// A member reads the global scope and its own organization.
    Org(OrgId),
    /// A reader that reads every organization. `own_org` is the organization
    /// that the reader belongs to, if any.
    EveryOrg { own_org: Option<OrgId> },
}

impl CatalogReadScope {
    pub fn can_read(&self, scope: &CatalogScope) -> bool {
        match (self, scope) {
            (_, CatalogScope::Global) | (Self::EveryOrg { .. }, CatalogScope::Org(_)) => true,
            (Self::Org(own), CatalogScope::Org(id)) => own == id,
            (Self::Unaffiliated, CatalogScope::Org(_)) => false,
        }
    }

    /// The organization that the reader belongs to, or `None` for a reader in
    /// no organization.
    pub fn own_org(&self) -> Option<&OrgId> {
        match self {
            Self::Org(id) => Some(id),
            Self::EveryOrg { own_org } => own_org.as_ref(),
            Self::Unaffiliated => None,
        }
    }

    /// Whether `id` is the reader's own organization.
    fn is_own(&self, id: &OrgId) -> bool {
        self.own_org() == Some(id)
    }

    /// Every scope that this reader can read, for a list filter.
    pub async fn readable_scopes(
        &self,
        orgs: &dyn OrgStore,
    ) -> anyhow::Result<BTreeSet<CatalogScope>> {
        let mut scopes = BTreeSet::from([CatalogScope::Global]);
        match self {
            Self::Unaffiliated => {}
            Self::Org(id) => {
                scopes.insert(CatalogScope::Org(id.clone()));
            }
            Self::EveryOrg { own_org } => {
                scopes.extend(own_org.clone().map(CatalogScope::Org));
                scopes.extend(
                    orgs.list_orgs()
                        .await?
                        .into_iter()
                        .map(|org| CatalogScope::Org(org.org_id)),
                );
            }
        }
        Ok(scopes)
    }
}

/// What one reader needs to show catalog names: its scopes, the live names of
/// its own organization, and the slugs of the other organizations.
#[derive(Debug, Clone)]
pub struct NameView {
    read: CatalogReadScope,
    /// The keys of the live entries in the reader's own organization. Each one
    /// shadows the global entry with the same key.
    shadowing: HashSet<String>,
    /// The slugs of the organizations other than the reader's own.
    slugs: HashMap<OrgId, OrgSlug>,
}

/// The rank of a digest hit for one reader: the reader's own organization,
/// global hand-made, global stable, global daily, then other organizations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum HitRank {
    OwnOrg,
    HandMade,
    Stable,
    Daily,
    OtherOrg,
}

impl NameView {
    pub fn new(
        read: CatalogReadScope,
        shadowing: impl IntoIterator<Item = CatalogName>,
        slugs: HashMap<OrgId, OrgSlug>,
    ) -> Self {
        Self {
            read,
            shadowing: shadowing.into_iter().map(|name| name.key()).collect(),
            slugs,
        }
    }

    /// Read the live names of the reader's own organization, and the slug of
    /// each other organization in `scopes` that the reader can read.
    pub async fn load<'a>(
        catalog: &dyn DescriptorCatalogStore,
        orgs: &dyn OrgStore,
        read: CatalogReadScope,
        scopes: impl IntoIterator<Item = &'a CatalogScope>,
    ) -> anyhow::Result<Self> {
        let shadowing = match read.own_org() {
            Some(org_id) => catalog.live_entries_in_org(org_id).await?,
            None => Vec::new(),
        };
        let foreign: BTreeSet<&OrgId> = scopes
            .into_iter()
            .filter(|scope| read.can_read(scope))
            .filter_map(CatalogScope::org_id)
            .filter(|id| !read.is_own(id))
            .collect();
        let slugs = if foreign.is_empty() {
            HashMap::new()
        } else {
            orgs.list_orgs()
                .await?
                .into_iter()
                .filter(|org| foreign.contains(&org.org_id))
                .map(|org| (org.org_id, org.slug))
                .collect()
        };
        Ok(Self::new(read, shadowing, slugs))
    }

    /// The name of an entry as this reader sees it, or `None` when the reader
    /// cannot read the entry's scope.
    ///
    /// - The reader's own organization's entry shows as the bare name.
    /// - A global entry that the reader's organization shadows shows as
    ///   `global/<name>`.
    /// - Another organization's entry shows as `<org-slug>/<name>`. An entry of
    ///   an organization that the roster does not hold reads as absent.
    /// - Any other global entry shows as the bare name.
    pub fn display_name(
        &self,
        scope: &CatalogScope,
        name: &CatalogName,
    ) -> Option<QualifiedCatalogName> {
        if !self.read.can_read(scope) {
            return None;
        }
        let qualifier = match scope {
            CatalogScope::Global if self.shadowing.contains(&name.key()) => {
                Some(CatalogQualifier::Global)
            }
            CatalogScope::Global => None,
            CatalogScope::Org(id) if self.read.is_own(id) => None,
            CatalogScope::Org(id) => Some(CatalogQualifier::Org(self.slugs.get(id)?.clone())),
        };
        Some(QualifiedCatalogName {
            qualifier,
            name: name.clone(),
        })
    }

    /// The live hits for one digest that this reader can read, each with its
    /// display name, in rank order. The first hit gives the name that a row
    /// shows. The other hits are synonyms: other names for the same digest.
    pub fn ranked(&self, hits: Vec<DigestHit>) -> Vec<(DigestHit, QualifiedCatalogName)> {
        let mut ranked: Vec<_> = hits
            .into_iter()
            .filter(|hit| hit.state != DescriptorEntryState::Tombstoned)
            .filter_map(|hit| {
                let shown = self.display_name(&hit.scope, &hit.name)?;
                Some((hit, shown))
            })
            .collect();
        ranked.sort_by_cached_key(|(hit, shown)| {
            let rank = match (&hit.scope, hit.kind) {
                (CatalogScope::Org(id), _) if self.read.is_own(id) => HitRank::OwnOrg,
                (CatalogScope::Org(_), _) => HitRank::OtherOrg,
                (CatalogScope::Global, DescriptorEntryKind::HandMade) => HitRank::HandMade,
                (CatalogScope::Global, DescriptorEntryKind::Stable) => HitRank::Stable,
                (CatalogScope::Global, DescriptorEntryKind::Daily) => HitRank::Daily,
            };
            // Another organization's name sorts by slug, then by name.
            let slug = match (rank, &shown.qualifier) {
                (HitRank::OtherOrg, Some(CatalogQualifier::Org(slug))) => slug.as_str().to_owned(),
                _ => String::new(),
            };
            (rank, slug, hit.name.key())
        });
        ranked
    }

    /// The names of each digest in `found`, as this reader sees them. The hit
    /// with the highest rank gives the display, and the other hits give the
    /// synonyms. A digest with no live hit that this reader can read has no
    /// key.
    pub fn displays(
        &self,
        found: HashMap<String, Vec<DigestHit>>,
    ) -> BTreeMap<String, DescriptorDisplay> {
        found
            .into_iter()
            .filter_map(|(digest, hits)| {
                let mut shown = self.ranked(hits).into_iter().map(|(hit, name)| {
                    let selector = member_selector(&name, &hit.member_key);
                    (selector, name)
                });
                let (display, name) = shown.next()?;
                let synonyms = shown.map(|(selector, _)| selector).collect();
                Some((
                    digest,
                    DescriptorDisplay {
                        display,
                        name: name.to_string(),
                        synonyms,
                    },
                ))
            })
            .collect()
    }
}

/// The member's [`MemberSelector`], with the entry's name as the reader sees
/// it.
pub fn member_selector(name: &QualifiedCatalogName, member_key: &DescriptorMemberKey) -> String {
    MemberSelector {
        name: name.clone(),
        member_key: member_key.clone(),
    }
    .to_string()
}

/// The names of each of `digests` that the reader can read, keyed by the
/// digest ([`NameView::displays`]).
pub async fn digest_displays(
    catalog: &dyn DescriptorCatalogStore,
    orgs: &dyn OrgStore,
    read: CatalogReadScope,
    digests: &[String],
) -> anyhow::Result<BTreeMap<String, DescriptorDisplay>> {
    if digests.is_empty() {
        return Ok(BTreeMap::new());
    }
    let found = catalog.find_by_digests(digests).await?;
    let scopes = found.values().flatten().map(|hit| &hit.scope);
    let view = NameView::load(catalog, orgs, read, scopes).await?;
    Ok(view.displays(found))
}

/// The live entry that a name selects for this reader, or `None`.
///
/// - A bare name selects the reader's own org entry, then the global entry.
/// - `global/<name>` selects the global entry.
/// - `<org-slug>/<name>` selects that organization's entry, if the reader can
///   read the organization.
///
/// A tombstoned entry, and an entry that the reader cannot read, select
/// nothing. So a tombstoned org entry does not hide the global entry.
pub async fn resolve(
    catalog: &dyn DescriptorCatalogStore,
    orgs: &dyn OrgStore,
    read: &CatalogReadScope,
    name: &QualifiedCatalogName,
) -> anyhow::Result<Option<DescriptorEntry>> {
    let scopes = match &name.qualifier {
        None => read
            .own_org()
            .map(|id| CatalogScope::Org(id.clone()))
            .into_iter()
            .chain([CatalogScope::Global])
            .collect(),
        Some(CatalogQualifier::Global) => vec![CatalogScope::Global],
        Some(CatalogQualifier::Org(slug)) => match orgs.lookup_org_by_slug(slug.as_str()).await? {
            Some(id) => vec![CatalogScope::Org(id)],
            None => Vec::new(),
        },
    };
    for scope in scopes.iter().filter(|scope| read.can_read(scope)) {
        if let Some(entry) = catalog.get_entry(scope, &name.name).await?
            && entry.state != DescriptorEntryState::Tombstoned
        {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use pipette_plan_types::{DescriptorEntryKind as Kind, DescriptorEntryState as State};

    use super::*;

    const ACME: &str = "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7e";
    const BETA: &str = "org_0195c3e2-a4f3-7b8c-9d1e-2f3a4b5c6d7f";

    fn org(id: &str) -> anyhow::Result<CatalogScope> {
        Ok(CatalogScope::Org(OrgId::try_new(id)?))
    }

    fn scope(word: &str) -> anyhow::Result<CatalogScope> {
        match word {
            "global" => Ok(CatalogScope::Global),
            "acme" => org(ACME),
            "beta" => org(BETA),
            other => anyhow::bail!("unknown test scope {other}"),
        }
    }

    fn hit(
        scope_word: &str,
        name: &str,
        kind: DescriptorEntryKind,
        state: DescriptorEntryState,
    ) -> anyhow::Result<DigestHit> {
        Ok(DigestHit {
            scope: scope(scope_word)?,
            name: CatalogName::try_new(name)?,
            kind,
            state,
            member_key: DescriptorMemberKey::Runtime { flavor: None },
        })
    }

    /// A reader: a member of `acme` or `beta`, a reader of no organization
    /// (`none`), or a reader of every organization (`every`).
    fn reader(word: &str) -> anyhow::Result<CatalogReadScope> {
        match word {
            "acme" => Ok(CatalogReadScope::Org(OrgId::try_new(ACME)?)),
            "beta" => Ok(CatalogReadScope::Org(OrgId::try_new(BETA)?)),
            "none" => Ok(CatalogReadScope::Unaffiliated),
            "every" => Ok(CatalogReadScope::EveryOrg { own_org: None }),
            "every_acme" => Ok(CatalogReadScope::EveryOrg {
                own_org: Some(OrgId::try_new(ACME)?),
            }),
            other => anyhow::bail!("unknown test reader {other}"),
        }
    }

    /// The view of one reader. `shadowed` holds the global names that the
    /// reader's organization shadows. `slugs` makes the slugs of both
    /// organizations known.
    fn view(who: &str, shadowed: &[&str], slugs: bool) -> anyhow::Result<NameView> {
        let shadowing = shadowed
            .iter()
            .map(|name| CatalogName::try_new(*name))
            .collect::<Result<Vec<_>, _>>()?;
        let slugs = if slugs {
            HashMap::from([
                (OrgId::try_new(ACME)?, OrgSlug::try_new("acme")?),
                (OrgId::try_new(BETA)?, OrgSlug::try_new("beta")?),
            ])
        } else {
            HashMap::new()
        };
        Ok(NameView::new(reader(who)?, shadowing, slugs))
    }

    #[rstest]
    #[case::member_reads_own_entry("acme", &["x"], true, "acme", "x", Some("x"))]
    #[case::member_sees_shadowed_global("acme", &["x"], true, "global", "x", Some("global/x"))]
    #[case::member_sees_unshadowed_global("acme", &["x"], true, "global", "y", Some("y"))]
    #[case::member_after_tombstone("acme", &[], true, "global", "x", Some("x"))]
    #[case::other_member_sees_global("beta", &[], true, "global", "x", Some("x"))]
    #[case::other_member_cannot_read("beta", &[], true, "acme", "x", None)]
    #[case::no_org_cannot_read("none", &[], true, "acme", "x", None)]
    #[case::outsider_sees_qualified("every", &[], true, "acme", "x", Some("acme/x"))]
    #[case::outsider_sees_bare_global("every", &[], true, "global", "x", Some("x"))]
    #[case::broad_member_sees_own_bare("every_acme", &[], true, "acme", "x", Some("x"))]
    #[case::broad_member_sees_other_qualified("every_acme", &[], true, "beta", "x", Some("beta/x"))]
    #[case::broad_member_sees_shadowed_global("every_acme", &["x"], true, "global", "x", Some("global/x"))]
    #[case::org_without_slug_is_absent("every", &[], false, "acme", "x", None)]
    fn display_name_follows_the_reader(
        #[case] who: &str,
        #[case] shadowed: &[&str],
        #[case] slugs: bool,
        #[case] entry_scope: &str,
        #[case] name: &str,
        #[case] shown: Option<&str>,
    ) -> anyhow::Result<()> {
        let view = view(who, shadowed, slugs)?;
        let got = view
            .display_name(&scope(entry_scope)?, &CatalogName::try_new(name)?)
            .map(|name| name.to_string());
        assert_eq!(got.as_deref(), shown);
        Ok(())
    }

    #[rstest]
    #[case::member("acme", &["hand"], &["x", "Alpha", "global/hand", "stable", "daily"])]
    #[case::outsider_with_every_org(
        "every",
        &[],
        &["Alpha", "hand", "stable", "daily", "acme/x", "beta/x"]
    )]
    #[case::affiliated_with_every_org(
        "every_acme",
        &["hand"],
        &["x", "Alpha", "global/hand", "stable", "daily", "beta/x"]
    )]
    #[case::other_member("beta", &[], &["x", "Alpha", "hand", "stable", "daily"])]
    #[case::no_org("none", &[], &["Alpha", "hand", "stable", "daily"])]
    fn ranked_hits_follow_the_reader(
        #[case] who: &str,
        #[case] shadowed: &[&str],
        #[case] expected: &[&str],
    ) -> anyhow::Result<()> {
        let hits = vec![
            hit("global", "daily", Kind::Daily, State::Active)?,
            hit("beta", "x", Kind::HandMade, State::Active)?,
            hit("global", "stable", Kind::Stable, State::Hidden)?,
            hit("global", "gone", Kind::HandMade, State::Tombstoned)?,
            hit("acme", "x", Kind::HandMade, State::Pinned)?,
            hit("global", "hand", Kind::HandMade, State::Active)?,
            hit("global", "Alpha", Kind::HandMade, State::Active)?,
        ];
        let shown: Vec<String> = view(who, shadowed, true)?
            .ranked(hits)
            .into_iter()
            .map(|(_, shown)| shown.to_string())
            .collect();
        assert_eq!(shown, expected);
        Ok(())
    }

    fn flavored(
        scope_word: &str,
        name: &str,
        kind: DescriptorEntryKind,
        state: DescriptorEntryState,
        flavor: &str,
    ) -> anyhow::Result<DigestHit> {
        Ok(DigestHit {
            member_key: DescriptorMemberKey::Runtime {
                flavor: Some(flavor.to_string()),
            },
            ..hit(scope_word, name, kind, state)?
        })
    }

    /// The display and synonyms of each digest, as one reader sees them.
    fn displayed(
        view: &NameView,
        found: Vec<(&str, Vec<DigestHit>)>,
    ) -> Vec<(String, String, String, Vec<String>)> {
        let found = found
            .into_iter()
            .map(|(digest, hits)| (digest.to_string(), hits))
            .collect();
        view.displays(found)
            .into_iter()
            .map(|(digest, shown)| (digest, shown.display, shown.name, shown.synonyms))
            .collect()
    }

    /// Two members of one entry show different selectors. The member key
    /// follows the name as the reader sees it, and the other names are
    /// synonyms in rank order. A tombstoned hit, and a digest with no hit the
    /// reader can read, show nothing.
    #[rstest]
    #[case::member(
        "acme",
        &["llamacpp-v0.7.0"],
        "acme-build::linux-x64",
        "acme-build",
        &["global/llamacpp-v0.7.0::linux-x64", "b11669::linux-x64"]
    )]
    #[case::outsider(
        "every",
        &[],
        "llamacpp-v0.7.0::linux-x64",
        "llamacpp-v0.7.0",
        &["b11669::linux-x64", "acme/acme-build::linux-x64"]
    )]
    #[case::other_member(
        "beta",
        &[],
        "llamacpp-v0.7.0::linux-x64",
        "llamacpp-v0.7.0",
        &["b11669::linux-x64"]
    )]
    fn displays_follow_the_reader(
        #[case] who: &str,
        #[case] shadowed: &[&str],
        #[case] display: &str,
        #[case] name: &str,
        #[case] synonyms: &[&str],
    ) -> anyhow::Result<()> {
        let cpu =
            |scope_word, name, kind, state| flavored(scope_word, name, kind, state, "linux-x64");
        let found = vec![
            (
                "cpu",
                vec![
                    cpu("global", "b11669", Kind::Daily, State::Active)?,
                    cpu("global", "gone", Kind::HandMade, State::Tombstoned)?,
                    cpu("acme", "acme-build", Kind::HandMade, State::Active)?,
                    cpu("global", "llamacpp-v0.7.0", Kind::Stable, State::Active)?,
                ],
            ),
            (
                "cuda",
                vec![flavored(
                    "global",
                    "llamacpp-v0.7.0",
                    Kind::Stable,
                    State::Active,
                    "linux-cuda-12-8",
                )?],
            ),
            (
                "acme-only",
                vec![cpu("acme", "private", Kind::HandMade, State::Active)?],
            ),
            (
                "tombstoned",
                vec![cpu("global", "gone", Kind::HandMade, State::Tombstoned)?],
            ),
        ];
        let shown = displayed(&view(who, shadowed, true)?, found);
        let cpu = shown
            .iter()
            .find(|(digest, ..)| digest == "cpu")
            .ok_or_else(|| anyhow::anyhow!("no display for the cpu digest in {shown:?}"))?;
        assert_eq!((cpu.1.as_str(), cpu.2.as_str()), (display, name));
        assert_eq!(cpu.3, synonyms);
        let cuda = shown
            .iter()
            .find(|(digest, ..)| digest == "cuda")
            .map(|(_, display, ..)| display.as_str());
        let cuda_name = if who == "acme" {
            "global/llamacpp-v0.7.0::linux-cuda-12-8"
        } else {
            "llamacpp-v0.7.0::linux-cuda-12-8"
        };
        assert_eq!(cuda, Some(cuda_name));
        assert_ne!(cuda, Some(cpu.1.as_str()));
        let digests: Vec<&str> = shown.iter().map(|(digest, ..)| digest.as_str()).collect();
        let acme_reads = matches!(who, "acme" | "every");
        assert_eq!(digests.contains(&"acme-only"), acme_reads);
        assert!(!digests.contains(&"tombstoned"));
        Ok(())
    }

    #[test]
    fn model_hit_uses_the_member_key_from_the_lookup() -> anyhow::Result<()> {
        let mut model = hit("global", "AcmeLM-VL-3B", Kind::HandMade, State::Active)?;
        model.member_key = DescriptorMemberKey::Model {
            format: "gguf".parse()?,
            quantization: "q4_0".parse()?,
        };
        assert_eq!(
            displayed(
                &view("none", &[], false)?,
                vec![("model-digest", vec![model])]
            ),
            vec![(
                "model-digest".to_string(),
                "AcmeLM-VL-3B::gguf::q4_0".to_string(),
                "AcmeLM-VL-3B".to_string(),
                vec![],
            )]
        );
        Ok(())
    }

    /// A reader of every organization lists the entries of its own
    /// organization, also when the roster no longer holds that organization.
    #[tokio::test]
    async fn every_org_lists_its_own_org_outside_the_roster() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let orgs = pipette_store_fs::LocalFsOrgStore::new(dir.path().to_path_buf());
        let beta = pipette_core::organization::Organization::new(
            OrgSlug::try_new("beta")?,
            pipette_plan_types::NonEmptyTrimmedString::try_new("beta")?,
            chrono::Utc::now(),
        );
        orgs.put_org(&beta).await?;
        let scopes = reader("every_acme")?.readable_scopes(&orgs).await?;
        assert_eq!(
            scopes,
            BTreeSet::from([
                CatalogScope::Global,
                org(ACME)?,
                CatalogScope::Org(beta.org_id),
            ])
        );
        Ok(())
    }

    /// Another organization's hits sort by slug, then by name. A slug that
    /// starts another slug sorts first.
    #[test]
    fn other_org_hits_sort_by_slug_then_name() -> anyhow::Result<()> {
        let view = NameView::new(
            CatalogReadScope::EveryOrg { own_org: None },
            Vec::new(),
            HashMap::from([
                (OrgId::try_new(ACME)?, OrgSlug::try_new("a")?),
                (OrgId::try_new(BETA)?, OrgSlug::try_new("a-b")?),
            ]),
        );
        let hits = vec![
            hit("beta", "x", Kind::HandMade, State::Active)?,
            hit("acme", "y", Kind::HandMade, State::Active)?,
        ];
        let shown: Vec<String> = view
            .ranked(hits)
            .into_iter()
            .map(|(_, shown)| shown.to_string())
            .collect();
        assert_eq!(shown, ["a/y", "a-b/x"]);
        Ok(())
    }
}
