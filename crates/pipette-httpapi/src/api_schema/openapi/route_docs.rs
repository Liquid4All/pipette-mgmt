//! The prose of each operation and tag, from its file under `docs/api/`.

use super::Tag;

const STATUSES: &str = "\n## Statuses\n";

/// A file under `docs/api/`: its id, its repository path and its text.
type Doc = (&'static str, &'static str, &'static str);

/// A request body example: its `operationId`, media type, path and text.
type Example = (&'static str, &'static str, &'static str, &'static str);

/// Names the file once, so its path and the text it loads cannot disagree.
macro_rules! doc {
    ($id:literal, $path:literal) => {
        (
            $id,
            concat!("docs/api/", $path),
            include_str!(concat!("../../../../../docs/api/", $path)),
        )
    };
}

macro_rules! example {
    ($id:literal, $media:literal, $path:literal) => {
        (
            $id,
            $media,
            concat!("docs/api/", $path),
            include_str!(concat!("../../../../../docs/api/", $path)),
        )
    };
}

pub(super) struct RouteDoc {
    pub description: String,
    /// What each answer means for this route, by status.
    pub statuses: Vec<(u16, String)>,
}

pub(super) fn route_doc(operation_id: &str) -> Option<RouteDoc> {
    OPERATIONS
        .iter()
        .find(|(id, ..)| *id == operation_id)
        .map(|(_, path, text)| parse(path, text))
}

/// The operation's request body examples, as `(media type, text)`.
pub(super) fn examples(
    operation_id: &str,
) -> impl Iterator<Item = (&'static str, &'static str)> + Clone {
    EXAMPLES
        .iter()
        .filter(move |(id, ..)| *id == operation_id)
        .map(|(_, media, _, text)| (*media, *text))
}

/// The directory under `docs/api/` that holds the operation's file: its tag's slug.
pub(super) fn doc_dir(operation_id: &str) -> Option<&'static str> {
    let (.., path, _) = OPERATIONS.iter().find(|(id, ..)| *id == operation_id)?;
    path.strip_prefix("docs/api/")?.split('/').next()
}

/// The tag's intro, by its slug, without its title.
pub(super) fn tag_doc(slug: &str) -> Option<String> {
    TAGS.iter()
        .find(|(id, ..)| *id == slug)
        .map(|(_, path, text)| in_document_links(without_title(text).trim(), path))
}

fn parse(path: &str, text: &str) -> RouteDoc {
    let body = without_title(text);
    let (prose, table) = body.split_once(STATUSES).unwrap_or((body, ""));
    let statuses = table
        .lines()
        .filter_map(|row| {
            let mut cells = row.trim().strip_prefix('|')?.strip_suffix('|')?.split('|');
            let status = cells.next()?.trim().parse().ok()?;
            let when = cells.collect::<Vec<_>>().join("|");
            Some((status, in_document_links(when.trim(), path)))
        })
        .collect();
    RouteDoc {
        description: in_document_links(prose.trim(), path),
        statuses,
    }
}

/// `text` past its first line, the `# Title` a reader on GitHub sees.
fn without_title(text: &str) -> &str {
    text.split_once('\n').map_or("", |(_, rest)| rest)
}

/// `text` with each relative Markdown link, resolved from `path`, the file it
/// is in, turned into a link inside the document or into its label alone,
/// since a reader of the document has no checkout to follow it in. Fenced code
/// keeps its text.
fn in_document_links(text: &str, path: &str) -> String {
    let mut fenced = false;
    text.split_inclusive('\n')
        .map(|line| {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                return line.to_owned();
            }
            if fenced {
                return line.to_owned();
            }
            let mut out = String::with_capacity(line.len());
            let mut rest = line;
            while let Some((before, after)) = rest.split_once("](") {
                let end = after.find(')').unwrap_or(after.len());
                let (target, tail) = after.split_at(end);
                if let Some(href) = resolve(target, path) {
                    out.push_str(before);
                    out.push_str("](");
                    out.push_str(&href);
                    rest = tail;
                } else {
                    // A label sits on one line and holds no `[`, which
                    // `test_each_link_label_is_one_flat_line` checks.
                    let (text, label) = before.rsplit_once('[').unwrap_or((before, ""));
                    // An image's `!` goes with its link.
                    out.push_str(text.strip_suffix('!').unwrap_or(text));
                    out.push_str(label);
                    rest = tail.strip_prefix(')').unwrap_or(tail);
                }
            }
            out.push_str(rest);
            out
        })
        .collect()
}

/// The repository path a relative link to `target` from the file at `path`
/// names (that file itself, for a bare `#anchor`), and its anchor.
fn repository_file<'t>(target: &'t str, path: &str) -> (String, &'t str) {
    let (file, anchor) = target.split_once('#').unwrap_or((target, ""));
    let mut segments: Vec<&str> = path.split('/').collect();
    segments.pop();
    if file.is_empty() {
        segments.extend(path.rsplit('/').next());
    }
    file.split('/').for_each(|segment| match segment {
        "" | "." => {}
        ".." => {
            segments.pop();
        }
        name => segments.push(name),
    });
    (segments.join("/"), anchor)
}

/// Where a link to `target` from `path` lands: its operation's or tag's section
/// (anchor dropped), [`Tag::Document`] for a [`SHARED_RULES`] section, or a web
/// link as is. `None` otherwise, and the caller keeps the label.
fn resolve(target: &str, path: &str) -> Option<String> {
    if target.contains("://") || target.starts_with("mailto:") {
        return Some(target.to_owned());
    }
    let (file, anchor) = repository_file(target, path);
    if SHARED_RULES.contains(&(file.as_str(), anchor)) {
        return Some(Tag::Document.fragment());
    }
    if let Some((id, ..)) = OPERATIONS.iter().find(|(_, doc, _)| *doc == file) {
        // `docs/api/<tag slug>/<operationId>.md`
        let tag = Tag::by_slug(file.split('/').nth(2)?)?;
        return Some(format!("{}/{id}", tag.fragment()));
    }
    let (slug, ..) = TAGS.iter().find(|(_, doc, _)| *doc == file)?;
    Tag::by_slug(slug).map(Tag::fragment)
}

/// The `docs/httpapi.md` sections the document's description carries, which a
/// link to lands on [`Tag::Document`], the tag that points there.
const SHARED_RULES: &[(&str, &str)] = &[
    ("docs/httpapi.md", "12-request-size-limits"),
    ("docs/httpapi.md", "13-pagination"),
    ("docs/httpapi.md", "14-names"),
    ("docs/httpapi.md", "15-the-openapi-document"),
    ("docs/httpapi.md", "16-write-answers"),
];

/// Each operation's file, by its `operationId`.
const OPERATIONS: &[Doc] = &[
    doc!(
        "list_descriptor_catalog",
        "descriptor-catalog/list_descriptor_catalog.md"
    ),
    doc!(
        "list_catalog_import_runs",
        "descriptor-catalog/list_catalog_import_runs.md"
    ),
    doc!(
        "list_descriptor_catalog_for_device",
        "descriptor-catalog/list_descriptor_catalog_for_device.md"
    ),
    doc!(
        "lookup_descriptor_names",
        "descriptor-catalog/lookup_descriptor_names.md"
    ),
    doc!(
        "lookup_descriptor_digests",
        "descriptor-catalog/lookup_descriptor_digests.md"
    ),
    doc!(
        "create_org_descriptor_entry",
        "descriptor-catalog/create_org_descriptor_entry.md"
    ),
    doc!(
        "add_org_descriptor_member",
        "descriptor-catalog/add_org_descriptor_member.md"
    ),
    doc!(
        "change_org_descriptor_entry_state",
        "descriptor-catalog/change_org_descriptor_entry_state.md"
    ),
    doc!("create_campaign", "campaigns/create_campaign.md"),
    doc!("delete_campaign", "campaigns/delete_campaign.md"),
    doc!("edit_variants", "campaigns/edit_variants.md"),
    doc!("file_plan", "campaigns/file_plan.md"),
    doc!("get_campaign", "campaigns/get_campaign.md"),
    doc!(
        "get_campaign_overview",
        "campaigns/get_campaign_overview.md"
    ),
    doc!("get_campaign_plan", "campaigns/get_campaign_plan.md"),
    doc!("list_campaign_owners", "campaigns/list_campaign_owners.md"),
    doc!("list_campaign_plans", "campaigns/list_campaign_plans.md"),
    doc!("list_campaigns", "campaigns/list_campaigns.md"),
    doc!("list_variants", "campaigns/list_variants.md"),
    doc!("read_document", "campaigns/read_document.md"),
    doc!("recalculate_jobs", "campaigns/recalculate_jobs.md"),
    doc!("rerun_jobs", "campaigns/rerun_jobs.md"),
    doc!("revert_campaign", "campaigns/revert_campaign.md"),
    doc!("revise_document", "campaigns/revise_document.md"),
    doc!("set_campaign_owner", "campaigns/set_campaign_owner.md"),
    doc!("update_campaign", "campaigns/update_campaign.md"),
    doc!(
        "amend_client_permissions",
        "devices/amend_client_permissions.md"
    ),
    doc!("approve_client", "devices/approve_client.md"),
    doc!("assign_clients_to_org", "devices/assign_clients_to_org.md"),
    doc!("delete_client", "devices/delete_client.md"),
    doc!(
        "get_capability_vocabulary",
        "devices/get_capability_vocabulary.md"
    ),
    doc!("get_client", "devices/get_client.md"),
    doc!("list_client_owners", "devices/list_client_owners.md"),
    doc!(
        "list_client_preauth_keys",
        "devices/list_client_preauth_keys.md"
    ),
    doc!("list_clients", "devices/list_clients.md"),
    doc!("list_org_clients", "devices/list_org_clients.md"),
    doc!(
        "mint_client_preauth_key",
        "devices/mint_client_preauth_key.md"
    ),
    doc!("reject_client", "devices/reject_client.md"),
    doc!(
        "revoke_client_preauth_key",
        "devices/revoke_client_preauth_key.md"
    ),
    doc!("set_client_owner", "devices/set_client_owner.md"),
    doc!(
        "set_client_permissions",
        "devices/set_client_permissions.md"
    ),
    doc!("unsuspend_client", "devices/unsuspend_client.md"),
    doc!("update_client", "devices/update_client.md"),
    doc!("openapi_document", "document/openapi_document.md"),
    doc!("create_model_params", "model-params/create_model_params.md"),
    doc!("delete_model_params", "model-params/delete_model_params.md"),
    doc!("list_model_params", "model-params/list_model_params.md"),
    doc!("update_model_params", "model-params/update_model_params.md"),
    doc!("create_org", "orgs/create_org.md"),
    doc!("delete_org_benchmark", "orgs/delete_org_benchmark.md"),
    doc!("get_org", "orgs/get_org.md"),
    doc!("get_org_benchmark", "orgs/get_org_benchmark.md"),
    doc!("get_org_overview", "orgs/get_org_overview.md"),
    doc!("list_audit", "orgs/list_audit.md"),
    doc!("list_org_audit", "orgs/list_org_audit.md"),
    doc!("list_org_benchmarks", "orgs/list_org_benchmarks.md"),
    doc!("list_org_users", "orgs/list_org_users.md"),
    doc!("list_orgs", "orgs/list_orgs.md"),
    doc!("put_org_benchmark", "orgs/put_org_benchmark.md"),
    doc!("update_org", "orgs/update_org.md"),
    doc!("cancel_plan", "plans/cancel_plan.md"),
    doc!("explain_plan", "plans/explain_plan.md"),
    doc!("get_plan", "plans/get_plan.md"),
    doc!("get_plan_document", "plans/get_plan_document.md"),
    doc!("get_plan_results_csv", "plans/get_plan_results_csv.md"),
    doc!("get_plan_variants", "plans/get_plan_variants.md"),
    doc!("list_job_plans", "plans/list_job_plans.md"),
    doc!("list_plan_jobs", "plans/list_plan_jobs.md"),
    doc!("list_plans", "plans/list_plans.md"),
    doc!("update_plan", "plans/update_plan.md"),
    doc!("validate_plan", "plans/validate_plan.md"),
    doc!("add_own_key", "users/add_own_key.md"),
    doc!("amend_permissions", "users/amend_permissions.md"),
    doc!("create_user", "users/create_user.md"),
    doc!("get_own_user", "users/get_own_user.md"),
    doc!("get_user", "users/get_user.md"),
    doc!("list_preauth_keys", "users/list_preauth_keys.md"),
    doc!("list_user_keys", "users/list_user_keys.md"),
    doc!("list_users", "users/list_users.md"),
    doc!("mint_preauth_key", "users/mint_preauth_key.md"),
    doc!("register_clerk_key", "users/register_clerk_key.md"),
    doc!("register_user_key", "users/register_user_key.md"),
    doc!("revoke_own_key", "users/revoke_own_key.md"),
    doc!("revoke_preauth_key", "users/revoke_preauth_key.md"),
    doc!("revoke_user_key", "users/revoke_user_key.md"),
    doc!("set_permissions", "users/set_permissions.md"),
    doc!("set_user_org", "users/set_user_org.md"),
    doc!("update_user", "users/update_user.md"),
    doc!("delete_resource", "vault/delete_resource.md"),
    doc!("get_resource", "vault/get_resource.md"),
    doc!("head_blob", "vault/head_blob.md"),
    doc!("list_resources", "vault/list_resources.md"),
    doc!("put_blob", "vault/put_blob.md"),
    doc!("types", "vault/types.md"),
    doc!("upload_resources", "vault/upload_resources.md"),
];

/// Each tag's intro, by its slug.
const TAGS: &[Doc] = &[
    doc!("descriptor-catalog", "descriptor-catalog.md"),
    doc!("campaigns", "campaigns.md"),
    doc!("devices", "devices.md"),
    doc!("model-params", "model-params.md"),
    doc!("orgs", "orgs.md"),
    doc!("plans", "plans.md"),
    doc!("users", "users.md"),
    doc!("vault", "vault.md"),
];

/// Each request body example beside an operation's file, as
/// `<operationId>.example.<json|toml>`.
const EXAMPLES: &[Example] = &[
    example!(
        "add_own_key",
        "application/json",
        "users/add_own_key.example.json"
    ),
    example!(
        "amend_client_permissions",
        "application/json",
        "devices/amend_client_permissions.example.json"
    ),
    example!(
        "amend_permissions",
        "application/json",
        "users/amend_permissions.example.json"
    ),
    example!(
        "assign_clients_to_org",
        "application/json",
        "devices/assign_clients_to_org.example.json"
    ),
    example!(
        "create_campaign",
        "application/toml",
        "campaigns/plan-document.example.toml"
    ),
    example!(
        "create_model_params",
        "application/json",
        "model-params/create_model_params.example.json"
    ),
    example!(
        "create_org",
        "application/json",
        "orgs/create_org.example.json"
    ),
    example!(
        "create_user",
        "application/json",
        "users/create_user.example.json"
    ),
    example!(
        "edit_variants",
        "application/toml",
        "campaigns/edit_variants.example.toml"
    ),
    example!(
        "file_plan",
        "application/toml",
        "campaigns/plan-document.example.toml"
    ),
    example!(
        "mint_client_preauth_key",
        "application/json",
        "devices/mint_client_preauth_key.example.json"
    ),
    example!(
        "mint_preauth_key",
        "application/json",
        "users/mint_preauth_key.example.json"
    ),
    example!(
        "put_org_benchmark",
        "application/json",
        "orgs/put_org_benchmark.example.json"
    ),
    example!(
        "put_org_benchmark",
        "application/toml",
        "orgs/put_org_benchmark.example.toml"
    ),
    example!(
        "recalculate_jobs",
        "application/json",
        "campaigns/recalculate_jobs.example.json"
    ),
    example!(
        "register_clerk_key",
        "application/json",
        "users/register_clerk_key.example.json"
    ),
    example!(
        "register_user_key",
        "application/json",
        "users/register_user_key.example.json"
    ),
    example!(
        "rerun_jobs",
        "application/json",
        "campaigns/rerun_jobs.example.json"
    ),
    example!(
        "revert_campaign",
        "application/json",
        "campaigns/revert_campaign.example.json"
    ),
    example!(
        "revise_document",
        "application/toml",
        "campaigns/plan-document.example.toml"
    ),
    example!(
        "set_campaign_owner",
        "application/json",
        "campaigns/set_campaign_owner.example.json"
    ),
    example!(
        "set_client_owner",
        "application/json",
        "devices/set_client_owner.example.json"
    ),
    example!(
        "set_client_permissions",
        "application/json",
        "devices/set_client_permissions.example.json"
    ),
    example!(
        "set_permissions",
        "application/json",
        "users/set_permissions.example.json"
    ),
    example!(
        "set_user_org",
        "application/json",
        "users/set_user_org.example.json"
    ),
    example!(
        "update_campaign",
        "application/json",
        "campaigns/update_campaign.example.json"
    ),
    example!(
        "update_client",
        "application/json",
        "devices/update_client.example.json"
    ),
    example!(
        "update_model_params",
        "application/json",
        "model-params/update_model_params.example.json"
    ),
    example!(
        "update_org",
        "application/json",
        "orgs/update_org.example.json"
    ),
    example!(
        "update_plan",
        "application/json",
        "plans/update_plan.example.json"
    ),
    example!(
        "update_user",
        "application/json",
        "users/update_user.example.json"
    ),
    example!(
        "upload_resources",
        "application/json",
        "vault/upload_resources.example.json"
    ),
    example!(
        "validate_plan",
        "application/toml",
        "campaigns/plan-document.example.toml"
    ),
];

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use rstest::rstest;

    use super::*;
    use crate::api_schema::openapi::table::operations;

    #[rstest]
    #[case::an_operation("../campaigns/file_plan.md", Some("#tag/campaigns/file_plan"))]
    #[case::a_section_of_one("list_plan_jobs.md#x", Some("#tag/plans/list_plan_jobs"))]
    #[case::a_tag("../plans.md#x", Some("#tag/plans"))]
    #[case::a_slug_unlike_its_name("../orgs.md", Some("#tag/orgs"))]
    #[case::outside_api("../../storage.md#5-warehouse-contract", None)]
    #[case::same_file("#statuses", Some("#tag/plans/get_plan"))]
    #[case::shared_rules("../../httpapi.md#13-pagination", Some("#tag/document"))]
    #[case::other_httpapi("../../httpapi.md#2-endpoints", None)]
    #[case::web("https://example.test/x", Some("https://example.test/x"))]
    #[case::mail("mailto:a@example.test", Some("mailto:a@example.test"))]
    fn test_links_resolve_in_the_document(#[case] target: &str, #[case] resolved: Option<&str>) {
        assert_eq!(
            resolve(target, "docs/api/plans/get_plan.md").as_deref(),
            resolved
        );
    }

    #[test]
    fn test_a_route_doc_splits_its_prose_from_its_statuses() {
        let doc = parse(
            "docs/api/x/y.md",
            "# `GET /a`\n\nReads [a](../plans.md).\n\n## Statuses\n\n| Status | When |\n\
             |--------|------|\n| 200 | The [a](../x.md) |\n| 404 | No a \\| b |\n",
        );
        assert_eq!(doc.description, "Reads [a](#tag/plans).");
        assert_eq!(
            doc.statuses,
            vec![(200, "The a".to_owned()), (404, "No a \\| b".to_owned())]
        );
    }

    #[test]
    fn test_fenced_code_keeps_its_links() {
        let text = "[a](../plans/get_plan.md)\n```\n[b](../y.md)\n```\nSee [`c`](../../c.md#d).";
        assert_eq!(
            in_document_links(text, "docs/api/p/q.md"),
            "[a](#tag/plans/get_plan)\n```\n[b](../y.md)\n```\nSee `c`."
        );
    }

    #[test]
    fn test_a_dropped_image_keeps_only_its_alt_text() {
        assert_eq!(
            in_document_links("See ![a chart](../../c.png).", "docs/api/p/q.md"),
            "See a chart."
        );
    }

    /// Each `docs/api/` file's lines outside fenced code, beside its path.
    fn prose_lines() -> impl Iterator<Item = (&'static str, &'static str)> {
        OPERATIONS.iter().chain(TAGS).flat_map(|&(_, path, text)| {
            let mut fenced = false;
            text.lines()
                .filter(move |line| {
                    let fence = line.trim_start().starts_with("```");
                    fenced ^= fence;
                    !fenced && !fence
                })
                .map(move |line| (path, line))
        })
    }

    /// Each link outside fenced code has its label on its own line, with no
    /// `[` or `]` inside, so the label alone can stand for a link the document
    /// drops.
    #[test]
    fn test_each_link_label_is_one_flat_line() {
        let tangled: Vec<String> = prose_lines()
            .flat_map(|(path, line)| {
                line.match_indices("](")
                    .filter_map(move |(at, _)| line.get(..at))
                    .filter(|before| {
                        before
                            .rsplit_once('[')
                            .is_none_or(|(_, label)| label.contains(']'))
                    })
                    .map(move |before| format!("{path}: {before}"))
            })
            .collect();
        assert!(tangled.is_empty(), "{tangled:#?}");
    }

    /// A link into `docs/api/` lands on a section of the document: only a link
    /// the document cannot follow is left as its label, never a mistyped route.
    #[test]
    fn test_each_link_into_the_api_docs_lands_on_a_section() {
        let dropped: Vec<String> = prose_lines()
            .flat_map(|(path, line)| {
                line.split("](")
                    .skip(1)
                    .map(|after| after.split_once(')').map_or(after, |(target, _)| target))
                    .filter(move |target| {
                        repository_file(target, path).0.starts_with("docs/api/")
                            && resolve(target, path).is_none()
                    })
                    .map(move |target| format!("{path}: {target}"))
            })
            .collect();
        assert!(dropped.is_empty(), "{dropped:#?}");
    }

    /// Each [`SHARED_RULES`] anchor is a heading of `docs/httpapi.md`, so a
    /// renumbered section fails here rather than leaving its links as labels.
    #[test]
    fn test_each_shared_rule_is_a_heading() -> anyhow::Result<()> {
        const HTTPAPI: &str = include_str!("../../../../../docs/httpapi.md");
        // Slugged as GitHub does: lowercase, punctuation dropped, spaces to `-`.
        let anchors: BTreeSet<String> = HTTPAPI
            .lines()
            .filter_map(|line| {
                let title = line
                    .strip_prefix('#')?
                    .trim_start_matches('#')
                    .strip_prefix(' ')?;
                Some(
                    title
                        .to_lowercase()
                        .chars()
                        .filter_map(|c| match c {
                            ' ' => Some('-'),
                            c if c.is_alphanumeric() || c == '-' || c == '_' => Some(c),
                            _ => None,
                        })
                        .collect(),
                )
            })
            .collect();
        let stale: Vec<&(&str, &str)> = SHARED_RULES
            .iter()
            .filter(|(file, anchor)| *file != "docs/httpapi.md" || !anchors.contains(*anchor))
            .collect();
        anyhow::ensure!(stale.is_empty(), "no such heading: {stale:?}");
        Ok(())
    }

    /// The tables of `users.md`'s Permissions section are the code's: each
    /// name's implied set, and each group's names.
    #[test]
    fn test_the_permission_tables_are_the_codes() -> anyhow::Result<()> {
        use strum::IntoEnumIterator;

        use pipette_core::user_permission::UserPermissionGroup;
        use pipette_plan_types::UserPermission;

        let section = between(
            text_of(TAGS, "users")?,
            "\n## Permissions\n",
            "\n### Reading a set\n",
        )?;
        // "| `name` | `a`, `b`: why |" rows, by name.
        let rows: BTreeMap<&str, BTreeSet<&str>> = section
            .lines()
            .filter_map(|line| {
                let mut cells = line.strip_prefix("| `")?.split(" | ");
                let name = cells.next()?.strip_suffix('`')?;
                let names = ticked(cells.next()?).collect();
                Some((name, names))
            })
            .collect();
        let named = |permissions: &mut dyn Iterator<Item = UserPermission>| -> BTreeSet<String> {
            permissions.map(|p| p.to_string()).collect()
        };
        let stated = |name: &str| -> BTreeSet<String> {
            rows.get(name)
                .into_iter()
                .flatten()
                .flat_map(|&name| {
                    match UserPermissionGroup::iter().find(|group| group.to_string() == name) {
                        Some(group) => named(&mut group.permissions().iter().copied()),
                        None => BTreeSet::from([name.to_owned()]),
                    }
                })
                .collect()
        };
        let wrong: Vec<String> = UserPermission::iter()
            .map(|permission| (permission.to_string(), named(&mut permission.implied())))
            .chain(UserPermissionGroup::iter().map(|group| {
                (
                    group.to_string(),
                    named(&mut group.permissions().iter().copied()),
                )
            }))
            .filter(|(name, code)| stated(name) != *code)
            .map(|(name, code)| {
                format!(
                    "users.md says {name} is {:?}, the code {code:?}",
                    stated(&name)
                )
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }

    fn text_of(docs: &[Doc], id: &str) -> anyhow::Result<&'static str> {
        docs.iter()
            .find(|(doc_id, ..)| *doc_id == id)
            .map(|(.., text)| *text)
            .ok_or_else(|| anyhow::anyhow!("no doc {id}"))
    }

    /// The text after `from` up to the next `to`.
    fn between<'t>(text: &'t str, from: &str, to: &str) -> anyhow::Result<&'t str> {
        text.split_once(from)
            .and_then(|(_, rest)| rest.split_once(to))
            .map(|(section, _)| section)
            .ok_or_else(|| anyhow::anyhow!("no {from:?} followed by {to:?}"))
    }

    /// The `code` spans of `text`.
    fn ticked(text: &str) -> impl Iterator<Item = &str> {
        text.split('`').skip(1).step_by(2)
    }

    /// The source before its `mod tests`, so a test fixture proves nothing; an
    /// error without the marker, since the whole file would quietly pass.
    fn code_before_tests(source: &'static str) -> anyhow::Result<&'static str> {
        source
            .split_once("\n#[cfg(test)]\nmod tests")
            .map(|(code, _)| code)
            .ok_or_else(|| anyhow::anyhow!("no `#[cfg(test)] mod tests` marker"))
    }

    #[test]
    fn test_the_plan_statuses_are_the_enum() -> anyhow::Result<()> {
        use strum::IntoEnumIterator;

        use pipette_plan_types::PlanStatus;

        let table = between(
            text_of(TAGS, "plans")?,
            "\n## Plan statuses\n",
            "\n**The counts.**",
        )?;
        let rows: BTreeSet<&str> = table
            .lines()
            .skip_while(|line| !line.starts_with("|---"))
            .filter_map(|line| Some(line.strip_prefix("| `")?.split_once('`')?.0))
            .collect();
        let statuses: BTreeSet<&str> = PlanStatus::iter().map(PlanStatus::label).collect();
        anyhow::ensure!(
            rows == statuses,
            "plans.md lists the statuses {rows:?}, PlanStatus has {statuses:?}"
        );
        Ok(())
    }

    #[test]
    fn test_the_device_statuses_are_the_enum() -> anyhow::Result<()> {
        use strum::IntoEnumIterator;

        use pipette_core::client::ClientStatus;

        let sentence = between(text_of(TAGS, "devices")?, "\n## Device status\n", ";")?;
        let stated: BTreeSet<String> = ticked(sentence).map(str::to_owned).collect();
        let statuses: BTreeSet<String> = ClientStatus::iter().map(|s| s.to_string()).collect();
        anyhow::ensure!(
            stated == statuses,
            "devices.md names the statuses {stated:?}, ClientStatus has {statuses:?}"
        );
        Ok(())
    }

    /// Each metric row names every metric the scorer means to write for its
    /// types, and no metric the scorer never writes.
    #[test]
    fn test_each_metric_row_is_the_scorer() -> anyhow::Result<()> {
        use pipette_pipeline::score::intended_metrics;
        use pipette_plan_types::BenchmarkType;

        let score = code_before_tests(include_str!("../../../../pipette-pipeline/src/score.rs"))?;

        let table = between(
            text_of(TAGS, "plans")?,
            "| `benchmark_type` | `metric` |",
            "\n\n",
        )?;
        // Per row, the types it is for and the metrics it ticks; units stay unticked.
        let rows: Vec<(BTreeSet<&str>, BTreeSet<&str>)> = table
            .lines()
            .filter_map(|line| line.strip_prefix("| ")?.split_once(" | "))
            .map(|(types, metrics)| (ticked(types).collect(), ticked(metrics).collect()))
            .collect();
        let named: BTreeSet<String> = rows
            .iter()
            .flat_map(|(types, _)| types.iter().map(|t| t.to_string()))
            .collect();
        let types: BTreeSet<String> = BenchmarkType::ALL.iter().map(|t| t.to_string()).collect();
        anyhow::ensure!(
            named == types,
            "plans.md has metric rows for {named:?}, BenchmarkType is {types:?}"
        );

        // A unit is quoted in score.rs too, so it must not pass as a metric.
        let units: BTreeSet<&str> = BenchmarkType::ALL
            .iter()
            .flat_map(|kind| intended_metrics(*kind, Some(true)))
            .map(|(_, unit)| *unit)
            .collect();
        BenchmarkType::ALL
            .iter()
            .try_for_each(|kind| -> anyhow::Result<()> {
                let name = kind.to_string();
                let ticks = rows
                    .iter()
                    .find(|(types, _)| types.contains(name.as_str()))
                    .map(|(_, ticks)| ticks)
                    .ok_or_else(|| anyhow::anyhow!("plans.md has no metric row for {name}"))?;
                // With speculation on, so `vl_spec_decode` intends all it can write.
                let intended: BTreeSet<&str> = intended_metrics(*kind, Some(true))
                    .iter()
                    .map(|(metric, _)| *metric)
                    .collect();
                let missing: Vec<&&str> = intended.difference(ticks).collect();
                anyhow::ensure!(
                    missing.is_empty(),
                    "plans.md's {name} row leaves out the intended metrics {missing:?}"
                );
                let unwritten: Vec<&&str> = ticks
                    .iter()
                    .filter(|m| {
                        !intended.contains(*m)
                            && (units.contains(*m) || !score.contains(&format!("\"{m}\"")))
                    })
                    .collect();
                anyhow::ensure!(
                    unwritten.is_empty(),
                    "plans.md's {name} row names metrics score.rs never writes: {unwritten:?}"
                );
                Ok(())
            })
    }

    /// Each refusal message the docs quote is a literal the auth code emits.
    #[test]
    fn test_the_refusals_quoted_are_the_codes() -> anyhow::Result<()> {
        let auth = code_before_tests(include_str!("../../auth.rs"))?;

        let users = between(
            text_of(TAGS, "users")?,
            "\n### Status, then permission\n",
            "\nThe first three ",
        )?;
        let devices = between(
            text_of(TAGS, "devices")?,
            "\n## Device status\n",
            "\nThe scheduler ",
        )?;
        // A message is a lowercase span with a space; a route (`GET /x`) is not.
        let quoted: BTreeSet<&str> = ticked(users)
            .chain(ticked(devices))
            .filter(|span| span.contains(' ') && span.starts_with(|c: char| c.is_ascii_lowercase()))
            .collect();
        anyhow::ensure!(!quoted.is_empty(), "no refusal message quoted");
        let unknown: Vec<&str> = quoted
            .into_iter()
            .filter(|message| !auth.contains(&format!("\"{message}\"")))
            .collect();
        anyhow::ensure!(
            unknown.is_empty(),
            "users.md or devices.md quotes refusals auth.rs never emits: {unknown:?}"
        );
        Ok(())
    }

    /// The fields the results doc says the withheld tier drops are the ones
    /// `withhold_fleet` changes in the body, for every failure source.
    #[test]
    fn test_the_withheld_tier_is_withhold_fleet() -> anyhow::Result<()> {
        use strum::IntoEnumIterator;

        use pipette_pipeline::plan_results::ResultRow;
        use pipette_plan_types::FailureSource;

        let tier = between(
            text_of(OPERATIONS, "get_plan_results_csv")?,
            "**The withheld tier.**",
            "\n\n",
        )?
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
        let (cleared, rest) = tier
            .split_once(": ")
            .ok_or_else(|| anyhow::anyhow!("no field list"))?;
        let cleared: BTreeSet<&str> = ticked(cleared).filter(|name| *name != "null").collect();
        let kept: BTreeSet<&str> =
            ticked(between(rest, "Of `failure_source`, only ", ", which")?).collect();
        let body = |row: ResultRow| -> anyhow::Result<serde_json::Value> {
            Ok(serde_json::to_value(pipette_plan_types::ResultRow::from(
                row,
            ))?)
        };
        let fields = |value: &serde_json::Value| -> BTreeSet<String> {
            value
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.clone())
                .collect()
        };
        FailureSource::iter().try_for_each(|source| -> anyhow::Result<()> {
            let row = failed_row(source)?;
            let mut withheld = row.clone();
            withheld.withhold_fleet();
            let (before, after) = (body(row)?, body(withheld)?);
            let changed: BTreeSet<String> = fields(&before)
                .union(&fields(&after))
                .filter(|field| before.get(field.as_str()) != after.get(field.as_str()))
                .cloned()
                .collect();
            let mut stated: BTreeSet<String> = cleared.iter().map(|&f| f.to_owned()).collect();
            if !kept.contains(source.as_ref()) {
                stated.insert("failure_source".to_owned());
            }
            anyhow::ensure!(
                changed == stated,
                "on a {source} failure, withhold_fleet changes {changed:?}, \
                 get_plan_results_csv.md says {stated:?}"
            );
            Ok(())
        })
    }

    /// A failed row with every field set, from `source`.
    fn failed_row(
        source: pipette_plan_types::FailureSource,
    ) -> anyhow::Result<pipette_pipeline::plan_results::ResultRow> {
        let at = |s: &str| s.parse::<chrono::DateTime<chrono::Utc>>();
        let text = |s: &str| Some(s.to_owned());
        let map = |k: &str, v: serde_json::Value| BTreeMap::from([(k.to_owned(), v)]);
        // A struct literal, so a field added to ResultRow must be set here
        // and a field withhold_fleet clears shows as changed.
        Ok(pipette_pipeline::plan_results::ResultRow {
            job_id: pipette_plan_types::JobId::try_new("job-1")?,
            benchmark_id: "bench".into(),
            benchmark_type: "eval".into(),
            metric: "accuracy".into(),
            value: Some(0.5),
            value_stddev: Some(0.25),
            unit: "ratio".into(),
            failed: Some(true),
            failed_reason: text("1 client (ev1_a): out of memory"),
            model_descriptor: text("{}"),
            model_descriptor_sha256: text(&"cd".repeat(32)),
            runtime_descriptor: text("{}"),
            runtime_descriptor_sha256: text(&"ab".repeat(32)),
            model_vault_descriptor: text("{}"),
            model_flags: text("{}"),
            runtime_flags: text("{}"),
            benchmark_flags: text("{}"),
            requested_model_flags: text("{}"),
            requested_runtime_flags: text("{}"),
            requested_benchmark_flags: text("{}"),
            eval_metadata: text("{\"k\":1}"),
            client_extras: text("{\"host\":\"bench-a\"}"),
            parameters: map("max_tokens", 8.into()),
            observations: map("max_swap_bytes", 0.into()),
            client_id: text("ev1_a"),
            device_name: text("Pixel 9"),
            device: map("os", "android".into()),
            client_version: text("1.4.0"),
            submitted_at: Some(at("2026-09-29T10:00:00Z")?),
            scored_at: Some(at("2026-09-29T10:05:00Z")?),
            device_time_ms: Some(9500),
            failure_source: Some(source),
            rescored_from: Some(pipette_plan_types::JobId::try_new("job-0")?),
            run_submitted_at: Some(at("2026-09-29T09:00:00Z")?),
            score_runtime_version: text("1.2.3"),
            model_params_total_millions: Some(350),
            model_params_active_millions: Some(350),
        })
    }

    /// Every file in a directory under `docs/api/`, as its repository path.
    fn files_under_api() -> anyhow::Result<BTreeSet<String>> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let directories = std::fs::read_dir(root.join("docs/api"))?
            .map(|entry| Ok(entry?.path()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        directories
            .iter()
            .filter(|directory| directory.is_dir())
            .map(|directory| -> anyhow::Result<Vec<String>> {
                std::fs::read_dir(directory)?
                    .map(|file| {
                        let path = file?.path();
                        Ok(path.strip_prefix(&root)?.to_string_lossy().into_owned())
                    })
                    .collect()
            })
            .collect::<anyhow::Result<Vec<_>>>()
            .map(|files| files.into_iter().flatten().collect())
    }

    /// Every operation has its file, titled with its route, and every file is
    /// an operation's, so the index and the directory cannot drift.
    #[test]
    fn test_each_operation_has_its_file() -> anyhow::Result<()> {
        let listed: BTreeSet<String> = OPERATIONS
            .iter()
            .map(|(_, path, _)| (*path).to_owned())
            .collect();
        let on_disk: BTreeSet<String> = files_under_api()?
            .into_iter()
            .filter(|path| Path::new(path).extension().is_some_and(|ext| ext == "md"))
            .collect();
        anyhow::ensure!(listed == on_disk, "the index and docs/api/ differ");
        let wrong: Vec<String> = operations()
            .iter()
            .filter_map(|operation| {
                let Some((_, path, text)) =
                    OPERATIONS.iter().find(|(id, ..)| *id == operation.handler)
                else {
                    return Some(format!("{} has no file", operation.handler));
                };
                let title = format!("# `{} {}`", operation.method, operation.path);
                if text.lines().next() != Some(title.as_str()) {
                    return Some(format!("{path} is not titled {title}"));
                }
                (!text.contains(STATUSES)).then(|| format!("{path} has no Statuses table"))
            })
            .collect();
        anyhow::ensure!(wrong.is_empty(), "{wrong:#?}");
        Ok(())
    }

    /// Every example file is listed by its path, so none sits unused.
    #[test]
    fn test_each_example_is_listed() -> anyhow::Result<()> {
        let listed: BTreeSet<String> = EXAMPLES
            .iter()
            .map(|(_, _, path, _)| (*path).to_owned())
            .collect();
        let on_disk: BTreeSet<String> = files_under_api()?
            .into_iter()
            .filter(|path| path.contains(".example."))
            .collect();
        anyhow::ensure!(
            listed == on_disk,
            "listed {listed:#?}, on disk {on_disk:#?}"
        );
        Ok(())
    }

    /// Every tag but the document's own has its intro.
    #[test]
    fn test_each_tag_has_its_intro() {
        let bare: Vec<&str> = <Tag as strum::IntoEnumIterator>::iter()
            .filter(|tag| !matches!(tag, Tag::Document))
            .filter(|tag| tag.description().is_empty())
            .map(Tag::name)
            .collect();
        assert!(bare.is_empty(), "tags with no intro: {bare:?}");
    }

    /// A `Statuses` row names a status the operation answers, so the prose
    /// cannot describe an answer the route never gives.
    #[test]
    fn test_statuses_are_answers_the_route_gives() -> anyhow::Result<()> {
        let undeclared: Vec<String> = operations()
            .iter()
            .flat_map(|operation| {
                let declared = operation.statuses();
                route_doc(operation.handler)
                    .map(|doc| doc.statuses)
                    .unwrap_or_default()
                    .into_iter()
                    .filter(move |(status, _)| !declared.contains(status))
                    .map(move |(status, _)| format!("{} {status}", operation.handler))
            })
            .collect();
        anyhow::ensure!(undeclared.is_empty(), "{undeclared:#?}");
        Ok(())
    }
}
