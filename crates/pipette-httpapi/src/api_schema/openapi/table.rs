//! The route table: each `/api/v1` operation `router.rs` serves, what it reads
//! and answers. Its tests hold it to `router.rs`; its prose is in `docs/api/`.

use axum::http::Method;
use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde_json::{Map, Value};

use pipette_plan_types as pt;
use pipette_plan_types::UserPermission::{
    CancelPlans, ManageAnyOrgs, ManageClients, ManageOrgs, ReadAnyPlans, ReadOrgs, ReadPlans,
};

use pipette_pipeline::variant_edit::VariantEdit;
use pipette_planner::{SchedulerPlan, SchedulerVariant};

use super::{
    Auth, Body, Operation, Payload, Tag, either, json, json_body, object, of, op, toml_body,
};
use crate::api_v1::plans::csv::columns_description;
use crate::gate::Gate::{Active, Needs};
use crate::router::{
    ADMINISTER_CAMPAIGN, CREATE_ORGS, EDIT_ORG_CATALOG, FILE_PLANS, READ_CAMPAIGN, READ_FLEET,
    READ_ORG_DEVICES,
};

/// [`Auth::Gated`] by the gate's expression, as `router.rs` spells it.
macro_rules! gated {
    ($gate:expr) => {
        Auth::Gated($gate, stringify!($gate))
    };
}

/// The progress stream that a route answers for `Accept: application/x-ndjson`.
/// The `result` line carries an `Answer`.
fn progress<Answer: JsonSchema>() -> Payload {
    Payload::Ndjson(of::<pt::IngestEvent<Answer>>())
}

const DRY_RUN_OR_PROGRESS: &str = "The dry run, for a TOML body with `dry_run=true`; or, for \
    `Accept: application/x-ndjson`, the progress stream, one event a line";

/// A document of `[[variants]]` alone, as a plan's variants read for another
/// document to append.
fn standalone_blocks_schema(generator: &mut SchemaGenerator) -> Schema {
    let variants = generator
        .subschema_for::<pt::NonEmptyVec<SchedulerVariant>>()
        .to_value();
    Schema::from(Map::from_iter([
        ("type".to_owned(), "object".into()),
        ("properties".to_owned(), object([("variants", variants)])),
        ("required".to_owned(), vec!["variants"].into()),
        ("additionalProperties".to_owned(), false.into()),
    ]))
}

/// Any JSON object: this document's own answer, which it does not describe.
fn any_object(_: &mut SchemaGenerator) -> Schema {
    Schema::try_from(object([("type", "object".into())])).unwrap_or_default()
}

/// The fleet listing's filters, which it reads as raw pairs.
const FLEET_FILTERS: &[(&str, &str)] = &[
    ("tag", "A device must carry every tag asked for."),
    (
        "requires",
        "Flags separated by whitespace; a device must carry every one.",
    ),
    (
        "any_of",
        "One group of flags each; a device must carry one flag of every group.",
    ),
    ("excludes", "Flags a device must carry none of."),
    (
        "not_all_of",
        "One group of flags each; a device carrying every flag of a group is left out.",
    ),
];

/// Every operation under `/api/v1`, in `router.rs`'s order.
#[expect(
    clippy::too_many_lines,
    reason = "one table, a row per operation the router serves"
)]
pub(super) fn operations() -> Vec<Operation> {
    use Method as M;
    use Tag::{
        Campaigns, DescriptorCatalog, Devices, Document, Models, Organizations, Plans, Users, Vault,
    };

    vec![
        op(
            M::GET,
            "/api/v1/openapi.json",
            "openapi_document",
            Document,
            Auth::Open,
        )
        .summary("This document")
        .empty(304, "Unchanged since the `ETag` the request names")
        .reply(
            200,
            "The OpenAPI document",
            [Payload::Json(vec![any_object])],
        ),
        op(
            M::GET,
            "/api/v1/clients",
            "list_clients",
            Devices,
            gated!(READ_FLEET),
        )
        .summary("List the fleet, a page at a time")
        .errors(&[404])
        .query::<pt::ClientListQuery>()
        .repeated(FLEET_FILTERS)
        .reply(
            200,
            "A page of devices",
            [json::<pt::ListPage<pt::ClientListRow>>()],
        ),
        op(
            M::GET,
            "/api/v1/clients/capabilities",
            "get_capability_vocabulary",
            Devices,
            gated!(READ_FLEET),
        )
        .summary("The capability flags the fleet carries")
        .query::<pt::CapabilityVocabularyQuery>()
        .reply(200, "The vocabulary", [json::<pt::CapabilityVocabulary>()]),
        op(
            M::GET,
            "/api/v1/clients/{client_id}",
            "get_client",
            Devices,
            gated!(READ_FLEET),
        )
        .summary("Read one device")
        .reply(200, "The device", [json::<pt::ClientDetail>()]),
        op(
            M::PATCH,
            "/api/v1/clients/{client_id}",
            "update_client",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Edit a device's label, details or contact")
        .human_only()
        .errors(&[409])
        .body(json_body::<pt::UpdateClientRequest>())
        .reply(
            200,
            "The device and its owner, and whether it changed",
            [json::<pt::Changed<pt::ClientView>>()],
        ),
        op(
            M::DELETE,
            "/api/v1/clients/{client_id}",
            "delete_client",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Delete a device")
        .human_only()
        .empty(204, "Deleted, or there was no such device"),
        op(
            M::POST,
            "/api/v1/clients/{client_id}/approve",
            "approve_client",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Approve a pending device")
        .human_only()
        .errors(&[409])
        .reply(
            200,
            "The device and its owner, whether it changed, and the status it left",
            [json::<pt::ClientApproved>()],
        ),
        op(
            M::POST,
            "/api/v1/clients/{client_id}/reject",
            "reject_client",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Reject a pending device")
        .human_only()
        .errors(&[409])
        .empty(204, "Rejected"),
        op(
            M::POST,
            "/api/v1/clients/{client_id}/unsuspend",
            "unsuspend_client",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Lift a device's suspension")
        .human_only()
        .reply(
            200,
            "The device and its owner, and whether a suspension was lifted",
            [json::<pt::Changed<pt::ClientView>>()],
        )
        .empty(
            204,
            "No device record answers to the id; its suspension is cleared",
        ),
        op(
            M::GET,
            "/api/v1/clients/{client_id}/owners",
            "list_client_owners",
            Devices,
            gated!(READ_FLEET),
        )
        .summary("The owners a device may be given")
        .human_only()
        .query::<pt::OwnerLookupQuery>()
        .reply(200, "The matching owners", [json::<pt::OwnerMatches>()]),
        op(
            M::PUT,
            "/api/v1/clients/{client_id}/permissions",
            "set_client_permissions",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Replace a device's permissions")
        .human_only()
        .body(json_body::<pt::SetClientPermissionsRequest>())
        .reply(
            200,
            "The device's permissions",
            [json::<pt::Changed<pt::ClientPermissions>>()],
        ),
        op(
            M::PATCH,
            "/api/v1/clients/{client_id}/permissions",
            "amend_client_permissions",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Grant and revoke a device's permissions")
        .human_only()
        .body(json_body::<pt::AmendClientPermissionsRequest>())
        .reply(
            200,
            "The device's permissions",
            [json::<pt::Changed<pt::ClientPermissions>>()],
        ),
        op(
            M::PUT,
            "/api/v1/clients/{client_id}/owner",
            "set_client_owner",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Give a device an owner, or none")
        .human_only()
        .body(json_body::<pt::SetClientOwnerRequest>())
        .reply(
            200,
            "The device and its owner, and whether it changed",
            [json::<pt::Changed<pt::ClientView>>()],
        ),
        op(
            M::POST,
            "/api/v1/users/register",
            "register_user_key",
            Users,
            Auth::Open,
        )
        .summary("Register a key: an invite's, or a new account's")
        .errors(&[401, 403, 409, 429])
        .body(json_body::<pt::RegisterUserKeyRequest>())
        .reply(201, "Registered", [json::<pt::UserKeyRegistered>()])
        .reply(
            200,
            "Already registered to this account",
            [json::<pt::UserKeyRegistered>()],
        ),
        op(
            M::POST,
            "/api/v1/users/register/clerk",
            "register_clerk_key",
            Users,
            Auth::Clerk,
        )
        .summary("Register a key through a Clerk sign-in")
        .errors(&[403, 404, 409])
        .body(json_body::<pt::RegisterClerkUserKeyRequest>())
        .reply(201, "Registered", [json::<pt::UserKeyRegistered>()]),
        op(
            M::GET,
            "/api/v1/users/me",
            "get_own_user",
            Users,
            Auth::Signed,
        )
        .summary("The signing account")
        .errors(&[403])
        .reply(200, "The account", [json::<pt::OwnUserView>()]),
        op(
            M::POST,
            "/api/v1/users/me/keys",
            "add_own_key",
            Users,
            gated!(Active),
        )
        .summary("Add a key to the signing account")
        .errors(&[409])
        .body(json_body::<pt::AddUserKeyRequest>())
        .reply(201, "Registered", [json::<pt::UserKeyRegistered>()]),
        op(
            M::DELETE,
            "/api/v1/users/me/keys/{key_id}",
            "revoke_own_key",
            Users,
            gated!(Active),
        )
        .summary("Revoke one of the signing account's keys")
        .empty(204, "Revoked"),
        op(
            M::PUT,
            "/api/v1/users/{user_id}/org",
            "set_user_org",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Set or clear an account's organization")
        .human_only()
        .errors(&[409])
        .body(json_body::<pt::SetUserOrgRequest>())
        .reply(
            200,
            "The membership, and whether it changed",
            [json::<pt::Changed<pt::OrgUserRow>>()],
        ),
        op(
            M::GET,
            "/api/v1/users",
            "list_users",
            Users,
            gated!(Needs(ReadOrgs)),
        )
        .summary("List the accounts in scope")
        .reply(200, "The accounts", [json::<pt::ListPage<pt::UserView>>()]),
        op(
            M::POST,
            "/api/v1/users",
            "create_user",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Create an account")
        .human_only()
        .errors(&[404, 409])
        .body(json_body::<pt::CreateUserRequest>())
        .reply(201, "The account", [json::<pt::UserView>()]),
        op(
            M::GET,
            "/api/v1/users/{user_id}",
            "get_user",
            Users,
            gated!(Needs(ReadOrgs)),
        )
        .summary("Read an account")
        .reply(200, "The account", [json::<pt::UserDetail>()]),
        op(
            M::PATCH,
            "/api/v1/users/{user_id}",
            "update_user",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Edit an account, or change its status")
        .errors(&[409])
        .human_only()
        .body(json_body::<pt::UpdateUserRequest>())
        .reply(
            200,
            "The account, and whether it changed",
            [json::<pt::Changed<pt::UserView>>()],
        ),
        op(
            M::PUT,
            "/api/v1/users/{user_id}/permissions",
            "set_permissions",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Replace an account's permissions")
        .human_only()
        .body(json_body::<pt::SetUserPermissionsRequest>())
        .reply(
            200,
            "The account, and whether it changed",
            [json::<pt::Changed<pt::UserView>>()],
        ),
        op(
            M::PATCH,
            "/api/v1/users/{user_id}/permissions",
            "amend_permissions",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Grant and revoke an account's permissions")
        .human_only()
        .body(json_body::<pt::AmendUserPermissionsRequest>())
        .reply(
            200,
            "What was granted and revoked",
            [json::<pt::UserPermissionsAmended>()],
        ),
        op(
            M::GET,
            "/api/v1/users/{user_id}/keys",
            "list_user_keys",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("List an account's keys")
        .human_only()
        .reply(200, "The keys", [json::<pt::ListPage<pt::UserKey>>()]),
        op(
            M::DELETE,
            "/api/v1/users/{user_id}/keys/{key_id}",
            "revoke_user_key",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Revoke one of an account's keys")
        .human_only()
        .empty(204, "Revoked"),
        op(
            M::GET,
            "/api/v1/users/{user_id}/preauth-keys",
            "list_preauth_keys",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("List an account's pre-auth keys")
        .human_only()
        .reply(
            200,
            "The pre-auth keys",
            [json::<pt::ListPage<pt::UserPreauthKeyView>>()],
        ),
        op(
            M::POST,
            "/api/v1/users/{user_id}/preauth-keys",
            "mint_preauth_key",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Mint a pre-auth key that registers a key on the account")
        .human_only()
        .errors(&[409])
        .body(json_body::<pt::MintUserPreauthRequest>())
        .reply(
            201,
            "The key, whose token is shown once",
            [json::<pt::UserPreauthKeyMinted>()],
        ),
        op(
            M::DELETE,
            "/api/v1/users/{user_id}/preauth-keys/{key_id}",
            "revoke_preauth_key",
            Users,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Revoke an account's pre-auth key")
        .human_only()
        .empty(204, "Revoked"),
        op(
            M::GET,
            "/api/v1/orgs",
            "list_orgs",
            Organizations,
            gated!(Needs(ReadOrgs)),
        )
        .summary("List the organizations in scope")
        .query::<pt::OrgListQuery>()
        .reply(
            200,
            "The organizations",
            [json::<pt::ListPage<pt::OrgView>>()],
        ),
        op(
            M::POST,
            "/api/v1/orgs",
            "create_org",
            Organizations,
            gated!(CREATE_ORGS),
        )
        .summary("Create an organization")
        .human_only()
        .errors(&[409])
        .body(json_body::<pt::CreateOrgRequest>())
        .reply(201, "The organization", [json::<pt::OrgView>()]),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}",
            "get_org",
            Organizations,
            gated!(Needs(ReadOrgs)),
        )
        .summary("Read an organization, by id or handle")
        .reply(200, "The organization", [json::<pt::OrgView>()]),
        op(
            M::PATCH,
            "/api/v1/orgs/{org_id}",
            "update_org",
            Organizations,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Rename an organization, or change its status")
        .human_only()
        .body(json_body::<pt::UpdateOrgRequest>())
        .reply(
            200,
            "The organization, and whether it changed",
            [json::<pt::Changed<pt::OrgView>>()],
        ),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/users",
            "list_org_users",
            Organizations,
            gated!(Needs(ReadOrgs)),
        )
        .summary("List an organization's members")
        .reply(200, "The members", [json::<pt::ListPage<pt::OrgUserRow>>()]),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/overview",
            "get_org_overview",
            Organizations,
            gated!(Needs(ReadOrgs)),
        )
        .summary("An organization's members, devices and campaigns at a glance")
        .reply(200, "The overview", [json::<pt::OrgOverview>()]),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/audit",
            "list_org_audit",
            Organizations,
            gated!(Needs(ManageOrgs)),
        )
        .summary("List what changed in an organization, newest first")
        .human_only()
        .query::<pt::AuditQuery>()
        .reply(
            200,
            "A page of events",
            [json::<pt::Named<pt::ListPage<pt::AuditEvent>>>()],
        ),
        op(
            M::GET,
            "/api/v1/descriptor-catalog",
            "list_descriptor_catalog",
            DescriptorCatalog,
            gated!(Active),
        )
        .summary("List descriptor catalog entries")
        .query::<pt::DescriptorCatalogQuery>()
        .errors(&[404])
        .reply(
            200,
            "A page of entries",
            [json::<pt::ListPage<pt::DescriptorEntryListRow>>()],
        ),
        op(
            M::GET,
            "/api/v1/descriptor-catalog/import-runs",
            "list_catalog_import_runs",
            DescriptorCatalog,
            gated!(READ_FLEET),
        )
        .summary("List recent catalog import and refresh runs")
        .query::<pt::CatalogImportRunsQuery>()
        .errors(&[404])
        .reply(
            200,
            "Recent runs and the last successful import",
            [json::<pt::CatalogImportRuns>()],
        ),
        op(
            M::GET,
            "/api/v1/descriptor-catalog/devices/{client_id}",
            "list_descriptor_catalog_for_device",
            DescriptorCatalog,
            gated!(READ_FLEET),
        )
        .summary("List catalog members offered to one device")
        .reply(
            200,
            "The whole short list and withheld report",
            [json::<pt::DescriptorCatalogShortList>()],
        ),
        op(
            M::GET,
            "/api/v1/descriptor-catalog/names",
            "lookup_descriptor_names",
            DescriptorCatalog,
            gated!(Active),
        )
        .summary("Look up descriptor catalog names")
        .query::<pt::DescriptorNameQuery>()
        .errors(&[404])
        .reply(
            200,
            "Found and unknown names",
            [json::<pt::DescriptorEntryLookup>()],
        ),
        op(
            M::GET,
            "/api/v1/descriptor-catalog/by-digest",
            "lookup_descriptor_digests",
            DescriptorCatalog,
            gated!(Active),
        )
        .summary("Look up names by descriptor digest")
        .query::<pt::DescriptorDigestQuery>()
        .errors(&[404])
        .reply(
            200,
            "Ranked names by digest",
            [json::<pt::DescriptorDigestNames>()],
        ),
        op(
            M::POST,
            "/api/v1/orgs/{org_id}/descriptor-catalog",
            "create_org_descriptor_entry",
            DescriptorCatalog,
            gated!(EDIT_ORG_CATALOG),
        )
        .summary("Create an organization descriptor catalog entry")
        .errors(&[409, 502])
        .body(json_body::<pt::CreateDescriptorEntryRequest>())
        .reply(
            201,
            "The created entry",
            [json::<pt::DescriptorEntryListRow>()],
        ),
        op(
            M::POST,
            "/api/v1/orgs/{org_id}/descriptor-catalog/{name}/members",
            "add_org_descriptor_member",
            DescriptorCatalog,
            gated!(EDIT_ORG_CATALOG),
        )
        .summary("Append a member to an organization catalog entry")
        .errors(&[409, 502])
        .body(json_body::<pt::AddDescriptorMemberRequest>())
        .reply(
            200,
            "The updated entry",
            [json::<pt::DescriptorEntryListRow>()],
        ),
        op(
            M::POST,
            "/api/v1/orgs/{org_id}/descriptor-catalog/{name}/state",
            "change_org_descriptor_entry_state",
            DescriptorCatalog,
            gated!(EDIT_ORG_CATALOG),
        )
        .summary("Change or tombstone an organization catalog entry")
        .errors(&[409])
        .body(json_body::<pt::ChangeDescriptorEntryStateRequest>())
        .empty(204, "The entry state changed"),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/benchmarks",
            "list_org_benchmarks",
            Organizations,
            gated!(Active),
        )
        .summary("List an organization's private benchmarks")
        .reply(200, "The catalog", [json::<pt::OrgBenchmarks>()]),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/benchmarks/{benchmark_id}",
            "get_org_benchmark",
            Organizations,
            gated!(Active),
        )
        .summary("Read a private benchmark, as JSON or TOML by its Accept header")
        .reply(
            200,
            "The benchmark",
            [
                Payload::Json(vec![pt::org_benchmark_schema]),
                // The catalog file, without the JSON answer's ids.
                Payload::Toml(of::<Map<String, Value>>()),
            ],
        ),
        op(
            M::PUT,
            "/api/v1/orgs/{org_id}/benchmarks/{benchmark_id}",
            "put_org_benchmark",
            Organizations,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Create or replace a private benchmark")
        .human_only()
        .errors(&[409, 412])
        .body(Body::JsonOrToml(of::<Map<String, Value>>()))
        .reply(
            200,
            "The benchmark as stored",
            [Payload::Json(vec![pt::org_benchmark_schema])],
        ),
        op(
            M::DELETE,
            "/api/v1/orgs/{org_id}/benchmarks/{benchmark_id}",
            "delete_org_benchmark",
            Organizations,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Delete a private benchmark")
        .human_only()
        .empty(204, "Deleted"),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/clients",
            "list_org_clients",
            Devices,
            gated!(READ_ORG_DEVICES),
        )
        .summary("List an organization's devices")
        .reply(200, "The devices", [json::<pt::ListPage<pt::ClientView>>()]),
        op(
            M::PUT,
            "/api/v1/orgs/{org_id}/clients",
            "assign_clients_to_org",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Give devices to an organization")
        .human_only()
        .body(json_body::<pt::AssignClientsRequest>())
        .reply(
            200,
            "Each device's outcome",
            [json::<pt::ClientsAssigned>()],
        ),
        op(
            M::GET,
            "/api/v1/orgs/{org_id}/client-preauth-keys",
            "list_client_preauth_keys",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("List the pre-auth keys that enroll devices into an organization")
        .human_only()
        .reply(
            200,
            "The pre-auth keys",
            [json::<pt::ListPage<pt::ClientPreauthKeyView>>()],
        ),
        op(
            M::POST,
            "/api/v1/orgs/{org_id}/client-preauth-keys",
            "mint_client_preauth_key",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Mint a pre-auth key that enrolls devices")
        .human_only()
        .body(json_body::<pt::MintClientPreauthRequest>())
        .reply(
            201,
            "The key, whose token is shown once",
            [json::<pt::ClientPreauthKeyMinted>()],
        ),
        op(
            M::DELETE,
            "/api/v1/orgs/{org_id}/client-preauth-keys/{key_id}",
            "revoke_client_preauth_key",
            Devices,
            gated!(Needs(ManageClients)),
        )
        .summary("Revoke a device pre-auth key")
        .human_only()
        .empty(204, "Revoked"),
        op(
            M::GET,
            "/api/v1/audit",
            "list_audit",
            Organizations,
            gated!(Needs(ManageAnyOrgs)),
        )
        .summary("List every audit event, newest first")
        .human_only()
        .errors(&[404])
        .query::<pt::AuditQuery>()
        .reply(
            200,
            "A page of events",
            [json::<pt::Named<pt::ListPage<pt::AuditEvent>>>()],
        ),
        op(
            M::GET,
            "/api/v1/model-params",
            "list_model_params",
            Models,
            gated!(Needs(ReadAnyPlans)),
        )
        .summary("List the models' parameter counts")
        .reply(200, "The rows", [json::<pt::ListPage<pt::ModelParams>>()]),
        op(
            M::POST,
            "/api/v1/model-params",
            "create_model_params",
            Models,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Add a model's parameter count")
        .errors(&[409])
        .human_only()
        .body(json_body::<pt::CreateModelParamsRequest>())
        .reply(201, "The row", [json::<pt::ModelParams>()]),
        op(
            M::PUT,
            "/api/v1/model-params/{model_key}",
            "update_model_params",
            Models,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Change a model's parameter count")
        .human_only()
        .body(json_body::<pt::UpdateModelParamsRequest>())
        .reply(
            200,
            "The row, and whether it changed",
            [json::<pt::Changed<pt::ModelParams>>()],
        ),
        op(
            M::DELETE,
            "/api/v1/model-params/{model_key}",
            "delete_model_params",
            Models,
            gated!(Needs(ManageOrgs)),
        )
        .summary("Remove a model's parameter count")
        .human_only()
        .never(&[404])
        .empty(204, "Removed, or there was no such row"),
        op(
            M::GET,
            "/api/v1/campaigns",
            "list_campaigns",
            Campaigns,
            gated!(READ_CAMPAIGN),
        )
        .summary("List the campaigns in scope, a page at a time")
        .query::<pt::OwnedListQuery>()
        .query::<pt::CampaignReadQuery>()
        .query::<pt::CampaignListQuery>()
        .reply(
            200,
            "A page of campaigns",
            [json::<pt::Named<pt::ListPage<pt::CampaignView>>>()],
        ),
        op(
            M::POST,
            "/api/v1/campaigns",
            "create_campaign",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Create a campaign from a plan document")
        .errors(&[429, 502])
        .query::<pt::CreateCampaignQuery>()
        .body(toml_body::<SchedulerPlan>())
        .reply(
            201,
            "The campaign and its first plan",
            [json::<pt::CampaignCreated>()],
        )
        .reply(
            200,
            DRY_RUN_OR_PROGRESS,
            [json::<pt::PlanDryRun>(), progress::<pt::CampaignAnswer>()],
        ),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}",
            "get_campaign",
            Campaigns,
            gated!(READ_CAMPAIGN),
        )
        .summary("Read a campaign")
        .query::<pt::CampaignReadQuery>()
        .reply(200, "The campaign", [json::<pt::Named<pt::CampaignView>>()]),
        op(
            M::PATCH,
            "/api/v1/campaigns/{campaign_id}",
            "update_campaign",
            Campaigns,
            gated!(ADMINISTER_CAMPAIGN),
        )
        .summary("Rename, label or archive a campaign")
        .body(json_body::<pt::UpdateCampaignRequest>())
        .reply(
            200,
            "The campaign, and whether it changed",
            [json::<pt::Changed<pt::CampaignView>>()],
        ),
        op(
            M::DELETE,
            "/api/v1/campaigns/{campaign_id}",
            "delete_campaign",
            Campaigns,
            gated!(ADMINISTER_CAMPAIGN),
        )
        .summary("Delete a campaign")
        .errors(&[409])
        .empty(204, "Deleted"),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/overview",
            "get_campaign_overview",
            Campaigns,
            gated!(READ_CAMPAIGN),
        )
        .summary("A campaign with its plans, for its page")
        .query::<pt::CampaignOverviewQuery>()
        .reply(
            200,
            "The campaign",
            [json::<pt::Named<pt::CampaignOverview>>()],
        ),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/owners",
            "list_campaign_owners",
            Campaigns,
            gated!(ADMINISTER_CAMPAIGN),
        )
        .summary("The owners a campaign may be handed to")
        .query::<pt::OwnerLookupQuery>()
        .reply(200, "The matching owners", [json::<pt::OwnerMatches>()]),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/plan",
            "get_campaign_plan",
            Campaigns,
            gated!(READ_CAMPAIGN),
        )
        .summary("A campaign's current plan")
        .query::<pt::PlanReadQuery>()
        .reply(200, "The plan", [json::<pt::Named<pt::PlanView>>()]),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/plans",
            "list_campaign_plans",
            Campaigns,
            gated!(READ_CAMPAIGN),
        )
        .summary("List a campaign's plans, newest first")
        .reply(200, "The plans", [json::<pt::CampaignPlans>()]),
        op(
            M::POST,
            "/api/v1/campaigns/{campaign_id}/plans",
            "file_plan",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("File a campaign's next plan")
        .errors(&[409, 429, 502])
        .query::<pt::CampaignPlanDocumentQuery>()
        .body(toml_body::<SchedulerPlan>())
        .reply(201, "The filed plan", [json::<pt::PlanFiled>()])
        .reply(
            200,
            DRY_RUN_OR_PROGRESS,
            [json::<pt::PlanDryRun>(), progress::<pt::PlanAnswer>()],
        ),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/variants",
            "list_variants",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("A campaign's variants, as its editor lists them")
        .errors(&[409])
        .reply(200, "The variants", [json::<pt::CampaignVariants>()]),
        op(
            M::POST,
            "/api/v1/campaigns/{campaign_id}/variants",
            "edit_variants",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Add, change or stop a campaign's variants")
        .errors(&[409, 429, 502])
        .query::<pt::ReviseCampaignQuery>()
        .body(toml_body::<VariantEdit>())
        .reply(201, "The revision filed", [json::<pt::CampaignRevised>()])
        .reply(
            200,
            DRY_RUN_OR_PROGRESS,
            [
                json::<pt::RevisionDryRun>(),
                progress::<pt::RevisionAnswer>(),
            ],
        ),
        op(
            M::GET,
            "/api/v1/campaigns/{campaign_id}/document",
            "read_document",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("A campaign's plan document, for its editor")
        .errors(&[409])
        .reply(200, "The document", [json::<pt::CampaignDocument>()]),
        op(
            M::PUT,
            "/api/v1/campaigns/{campaign_id}/document",
            "revise_document",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Revise a campaign by its whole plan document")
        .errors(&[409, 429, 502])
        .query::<pt::ReviseCampaignQuery>()
        .body(toml_body::<SchedulerPlan>())
        .reply(201, "The revision filed", [json::<pt::CampaignRevised>()])
        .reply(
            200,
            "The dry run, for `dry_run=true`; or the campaign unchanged, when the document is \
             the head's own; or, for `Accept: application/x-ndjson`, the progress stream, one \
             event a line",
            [
                either::<pt::RevisionDryRun, pt::Changed<pt::CampaignView>>(),
                progress::<pt::RevisionAnswer>(),
            ],
        ),
        op(
            M::POST,
            "/api/v1/campaigns/{campaign_id}/rerun",
            "rerun_jobs",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Run a campaign's jobs again")
        .errors(&[409])
        .query::<pt::RerunJobsQuery>()
        .body(json_body::<pt::RerunJobsRequest>())
        .reply(201, "The revision filed", [json::<pt::CampaignRevised>()])
        .reply(
            200,
            "The dry run, for `dry_run=true`",
            [json::<pt::RevisionDryRun>()],
        ),
        op(
            M::POST,
            "/api/v1/campaigns/{campaign_id}/revert",
            "revert_campaign",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Revert a campaign to an earlier plan of its history")
        .errors(&[409])
        .query::<pt::RevertCampaignQuery>()
        .body(json_body::<pt::RevertCampaignRequest>())
        .reply(201, "The revision filed", [json::<pt::CampaignRevised>()])
        .reply(
            200,
            "The dry run, for `dry_run=true`; or the campaign unchanged, when the plan's \
             document is the head's own",
            [either::<pt::RevisionDryRun, pt::Changed<pt::CampaignView>>()],
        ),
        op(
            M::POST,
            "/api/v1/campaigns/{campaign_id}/recalculate",
            "recalculate_jobs",
            Campaigns,
            gated!(FILE_PLANS),
        )
        .summary("Score a campaign's results again")
        .errors(&[409])
        .query::<pt::RerunJobsQuery>()
        .body(json_body::<pt::RecalculateJobsRequest>())
        .reply(201, "The revision filed", [json::<pt::CampaignRevised>()])
        .reply(
            200,
            "The dry run, for `dry_run=true`",
            [json::<pt::RevisionDryRun>()],
        ),
        op(
            M::PUT,
            "/api/v1/campaigns/{campaign_id}/owner",
            "set_campaign_owner",
            Campaigns,
            gated!(ADMINISTER_CAMPAIGN),
        )
        .summary("Hand a campaign to another owner")
        .errors(&[409])
        .body(json_body::<pt::SetCampaignOwnerRequest>())
        .reply(
            200,
            "The campaign, and whether it changed",
            [json::<pt::Changed<pt::CampaignView>>()],
        ),
        op(
            M::PUT,
            "/api/v1/vault/blobs/{principal_id}/{sha256}",
            "put_blob",
            Vault,
            gated!(Active),
        )
        .summary("Upload a file by its digest")
        .errors(&[408, 429])
        .never(&[404])
        .body(Body::Bytes)
        .reply(201, "Stored", [json::<pt::VaultBlobStored>()])
        .reply(
            200,
            "Stored already; the body was not read",
            [json::<pt::VaultBlobStored>()],
        ),
        op(
            M::HEAD,
            "/api/v1/vault/blobs/{principal_id}/{sha256}",
            "head_blob",
            Vault,
            gated!(Active),
        )
        .summary("Whether a file is uploaded")
        .empty(200, "Uploaded")
        .empty(404, "Not uploaded"),
        op(
            M::GET,
            "/api/v1/vault/types",
            "types",
            Vault,
            gated!(Active),
        )
        .summary("The model and runtime types the vault takes")
        .reply(200, "The types", [json::<pt::VaultTypes>()]),
        op(
            M::GET,
            "/api/v1/vault/resources",
            "list_resources",
            Vault,
            gated!(Active),
        )
        .summary("List the vault's resources in scope, a page at a time")
        .query::<pt::OwnedListQuery>()
        .query::<pt::VaultReadQuery>()
        .reply(
            200,
            "A page of resources",
            [json::<pt::Named<pt::ListPage<pt::VaultResourceDetail>>>()],
        ),
        op(
            M::POST,
            "/api/v1/vault/resources",
            "upload_resources",
            Vault,
            gated!(Active),
        )
        .summary("Record an uploaded model or runtime")
        .body(Body::Json(pt::upload_vault_resources_request_schema))
        .reply(
            201,
            "The resources recorded",
            [json::<pt::VaultResourcesUploaded>()],
        ),
        op(
            M::GET,
            "/api/v1/vault/resources/{resource_id}",
            "get_resource",
            Vault,
            gated!(Active),
        )
        .summary("Read a vault resource")
        .query::<pt::VaultReadQuery>()
        .reply(
            200,
            "The resource",
            [json::<pt::Named<pt::VaultResourceDetail>>()],
        ),
        op(
            M::DELETE,
            "/api/v1/vault/resources/{resource_id}",
            "delete_resource",
            Vault,
            gated!(Active),
        )
        .summary("Delete a vault resource")
        .empty(204, "Deleted"),
        op(
            M::GET,
            "/api/v1/plans",
            "list_plans",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("List the plans in scope, a page at a time")
        .query::<pt::PlanListQuery>()
        .reply(
            200,
            "A page of plans",
            [json::<pt::Named<pt::ListPage<pt::PlanView>>>()],
        ),
        op(
            M::POST,
            "/api/v1/plans/validate",
            "validate_plan",
            Plans,
            gated!(FILE_PLANS),
        )
        .summary("Check a plan document without filing it")
        .body(toml_body::<SchedulerPlan>())
        .reply(200, "What the check found", [json::<pt::PlanValidation>()]),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}",
            "get_plan",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("Read a plan")
        .query::<pt::PlanReadQuery>()
        .reply(200, "The plan", [json::<pt::Named<pt::PlanView>>()]),
        op(
            M::PATCH,
            "/api/v1/plans/{plan_id}",
            "update_plan",
            Plans,
            gated!(FILE_PLANS),
        )
        .summary("Change a plan's note")
        .body(json_body::<pt::UpdatePlanRequest>())
        .reply(
            200,
            "The plan, and whether its note changed",
            [json::<pt::Changed<pt::PlanView>>()],
        ),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}/document",
            "get_plan_document",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("The document a plan was filed from")
        .reply(
            200,
            "The plan document",
            [Payload::Toml(of::<SchedulerPlan>())],
        ),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}/variants",
            "get_plan_variants",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("A plan's variants, for another document to append")
        .reply(
            200,
            "The `[[variants]]` blocks; or, for `Accept: application/json`, one block per \
             variant, numbered as a variant edit's `restore` counts them",
            [
                Payload::Toml(standalone_blocks_schema),
                json::<pt::PlanVariants>(),
            ],
        ),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}/jobs",
            "list_plan_jobs",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("A plan's jobs, with their results when results=true")
        .query::<pt::PlanJobsQuery>()
        .reply(200, "The jobs", [json::<pt::Named<pt::PlanJobs>>()]),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}/results.csv",
            "get_plan_results_csv",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("A plan's results as CSV")
        .errors(&[502])
        .reply(
            200,
            "Every result row of the plan, a line each",
            [Payload::Csv(columns_description)],
        ),
        op(
            M::GET,
            "/api/v1/plans/{plan_id}/explain",
            "explain_plan",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("Why a plan's jobs wait: what each needs and who could run it")
        .reply(200, "The explanation", [json::<pt::PlanExplanation>()]),
        op(
            M::POST,
            "/api/v1/plans/{plan_id}/cancel",
            "cancel_plan",
            Plans,
            gated!(Needs(CancelPlans)),
        )
        .summary("Cancel a plan's open jobs")
        .errors(&[409])
        .reply(
            202,
            "The cancel, and what it found",
            [json::<pt::PlanCancelRequested>()],
        ),
        op(
            M::GET,
            "/api/v1/jobs/{job_id}/plans",
            "list_job_plans",
            Plans,
            gated!(Needs(ReadPlans)),
        )
        .summary("The plans in scope that asked for a job")
        .reply(200, "The plans", [json::<pt::JobPlans>()]),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use pipette_web_testkit::{RegisteredRoute, registered_routes};

    use super::*;

    /// The table names every `/api/v1` operation `router.rs` serves, and each
    /// as the router declares it: its handler, and the gate that admits.
    #[test]
    fn test_every_operation_is_described_as_the_router_serves_it() -> anyhow::Result<()> {
        let served: BTreeSet<RegisteredRoute> = registered_routes(include_str!("../../router.rs"))?
            .into_iter()
            .filter(|route| route.path.starts_with("/api/v1/"))
            .collect();
        let described: BTreeSet<RegisteredRoute> = operations()
            .into_iter()
            .map(|operation| RegisteredRoute {
                method: operation.method.as_str().to_owned(),
                path: operation.path.to_owned(),
                handler: operation.handler.to_owned(),
                gate: match operation.auth {
                    Auth::Gated(_, source) => Some(source.split_whitespace().collect()),
                    Auth::Open | Auth::Clerk | Auth::Signed => None,
                },
            })
            .collect();
        anyhow::ensure!(
            served.len() > 80,
            "the router scan found {} operations",
            served.len()
        );
        let unlisted: Vec<&RegisteredRoute> = served.difference(&described).collect();
        let stale: Vec<&RegisteredRoute> = described.difference(&served).collect();
        anyhow::ensure!(
            unlisted.is_empty() && stale.is_empty(),
            "`router.rs` alone: {unlisted:#?}\nthe table alone: {stale:#?}"
        );
        Ok(())
    }

    #[test]
    fn test_operation_ids_are_unique_and_every_operation_says_what_it_is() -> anyhow::Result<()> {
        let mut seen = BTreeSet::new();
        operations()
            .iter()
            .try_for_each(|operation| -> anyhow::Result<()> {
                anyhow::ensure!(
                    seen.insert(operation.handler),
                    "`{}` is two operations' id",
                    operation.handler
                );
                anyhow::ensure!(
                    !operation.summary.is_empty(),
                    "{} {} has no summary",
                    operation.method,
                    operation.path
                );
                anyhow::ensure!(
                    !operation.replies.is_empty(),
                    "{} {} declares no answer",
                    operation.method,
                    operation.path
                );
                Ok(())
            })
    }
}
