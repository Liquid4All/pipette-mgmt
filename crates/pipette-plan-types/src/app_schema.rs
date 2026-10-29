//! The bodies the console app reads and sends: the roots of its schema bundle
//! (`ui/src/lib/api-schema.json`), from which mgmt generates the app's
//! TypeScript and checks its own answers.

use schemars::{Schema, SchemaGenerator};

use crate::catalog::OrgBenchmarks;
use crate::catalog::schema::OrgBenchmark;
use crate::vault::schema::UploadVaultResourcesRequest;

/// Every body the app reads, into a serialize-side `generator`.
pub fn app_responses(generator: &mut SchemaGenerator) {
    generator.subschema_for::<crate::OwnUserView>();
    generator.subschema_for::<crate::UserKeyRegistered>();
    generator.subschema_for::<crate::ListPage<crate::OrgView>>();
    generator.subschema_for::<crate::OrgView>();
    generator.subschema_for::<crate::Changed<crate::OrgView>>();
    generator.subschema_for::<crate::ListPage<crate::UserView>>();
    generator.subschema_for::<crate::UserDetail>();
    generator.subschema_for::<crate::Changed<crate::UserView>>();
    // A set may hold names a newer release granted, so it stays `string[]`; this
    // is what the app checks the names it asks about against.
    generator.subschema_for::<crate::UserPermission>();
    generator.subschema_for::<crate::VaultResourcesUploaded>();
    generator.subschema_for::<crate::ClientPermission>();
    generator.subschema_for::<crate::ListPage<crate::ClientPreauthKeyView>>();
    generator.subschema_for::<crate::ClientPreauthKeyMinted>();
    generator.subschema_for::<crate::UserPermissionsAmended>();
    generator.subschema_for::<crate::ListPage<crate::UserKey>>();
    generator.subschema_for::<crate::ListPage<crate::UserPreauthKeyView>>();
    generator.subschema_for::<crate::UserPreauthKeyMinted>();
    generator.subschema_for::<crate::Changed<crate::OrgUserRow>>();
    generator.subschema_for::<OrgBenchmarks>();
    generator.subschema_for::<crate::Named<crate::ListPage<crate::VaultResourceDetail>>>();
    generator.subschema_for::<crate::Named<crate::VaultResourceDetail>>();
    generator.subschema_for::<crate::CampaignVariants>();
    generator.subschema_for::<crate::PlanVariants>();
    generator.subschema_for::<crate::CampaignDocument>();
    generator.subschema_for::<crate::ListPage<crate::ClientListRow>>();
    generator.subschema_for::<crate::Named<crate::ListPage<crate::AuditEvent>>>();
    generator.subschema_for::<crate::VaultTypes>();
    generator.subschema_for::<crate::ClientDetail>();
    generator.subschema_for::<crate::OwnerMatches>();
    generator.subschema_for::<crate::OrgOverview>();
    generator.subschema_for::<crate::ListPage<crate::PlanView>>();
    generator.subschema_for::<crate::Named<crate::ListPage<crate::PlanView>>>();
    generator.subschema_for::<crate::PlanView>();
    generator.subschema_for::<crate::Named<crate::PlanView>>();
    generator.subschema_for::<crate::Named<crate::PlanJobs>>();
    generator.subschema_for::<crate::PlanCancelRequested>();
    generator.subschema_for::<crate::Changed<crate::PlanView>>();
    generator.subschema_for::<crate::Named<crate::ListPage<crate::CampaignView>>>();
    generator.subschema_for::<crate::Named<crate::CampaignView>>();
    generator.subschema_for::<crate::CampaignCreated>();
    generator.subschema_for::<crate::PlanDryRun>();
    generator.subschema_for::<crate::PlanValidation>();
    generator.subschema_for::<crate::Named<crate::CampaignOverview>>();
    generator.subschema_for::<crate::RevisionDryRun>();
    generator.subschema_for::<crate::CampaignRevised>();
    // One line of the progress stream; `body` is the called route's answer.
    generator.subschema_for::<crate::IngestEvent<serde_json::Value>>();
    generator.subschema_for::<crate::ListPage<crate::ModelParams>>();
    generator.subschema_for::<crate::DescriptorEntry>();
    generator.subschema_for::<crate::ListPage<crate::DescriptorEntryListRow>>();
    generator.subschema_for::<crate::DescriptorCatalogShortList>();
    generator.subschema_for::<crate::DescriptorEntryLookup>();
    generator.subschema_for::<crate::DescriptorDigestNames>();
    generator.subschema_for::<crate::CatalogImportRuns>();
    generator.subschema_for::<crate::ModelParams>();
    generator.subschema_for::<crate::Changed<crate::ModelParams>>();
}

/// Every body the app sends, into a deserialize-side `generator`: a field with
/// a default is optional there.
pub fn app_requests(generator: &mut SchemaGenerator) {
    generator.subschema_for::<crate::CreateDescriptorEntryRequest>();
    generator.subschema_for::<crate::AddDescriptorMemberRequest>();
    generator.subschema_for::<crate::ChangeDescriptorEntryStateRequest>();
    generator.subschema_for::<crate::RegisterUserKeyRequest>();
    generator.subschema_for::<crate::AddUserKeyRequest>();
    generator.subschema_for::<crate::RegisterClerkUserKeyRequest>();
    generator.subschema_for::<crate::SetClientOwnerRequest>();
    generator.subschema_for::<UploadVaultResourcesRequest>();
    generator.subschema_for::<crate::AmendClientPermissionsRequest>();
    generator.subschema_for::<crate::MintClientPreauthRequest>();
    generator.subschema_for::<crate::CreateUserRequest>();
    generator.subschema_for::<crate::UpdateUserRequest>();
    generator.subschema_for::<crate::SetUserPermissionsRequest>();
    generator.subschema_for::<crate::AmendUserPermissionsRequest>();
    generator.subschema_for::<crate::MintUserPreauthRequest>();
    generator.subschema_for::<crate::SetUserOrgRequest>();
    generator.subschema_for::<crate::CreateOrgRequest>();
    generator.subschema_for::<crate::UpdateOrgRequest>();
    generator.subschema_for::<crate::UpdateCampaignRequest>();
    generator.subschema_for::<crate::UpdatePlanRequest>();
    generator.subschema_for::<crate::SetCampaignOwnerRequest>();
    generator.subschema_for::<crate::RerunJobsRequest>();
    generator.subschema_for::<crate::RecalculateJobsRequest>();
    generator.subschema_for::<crate::RevertCampaignRequest>();
    generator.subschema_for::<crate::CreateModelParamsRequest>();
    generator.subschema_for::<crate::UpdateModelParamsRequest>();
}

/// `POST /api/v1/vault/resources`'s body, by the mirror the app's bundle names:
/// the upload's own type is keyed by file role and cannot derive one.
pub fn upload_vault_resources_request_schema(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<UploadVaultResourcesRequest>()
}

/// One benchmark of an organization's catalog, by the mirror its listing names.
pub fn org_benchmark_schema(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<OrgBenchmark>()
}
