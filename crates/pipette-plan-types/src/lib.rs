//! Plan types shared across pipette clients.
//!
//! No module is public, so moving an item between files changes no path. Each
//! imports only from those declared above it; `ci/checks/plan-types-layering.py`
//! enforces the order.
//!
//! An `/api/v1` body or query is published under its Rust name: subject, then
//! role, a qualifier allowed first (`OwnUserView`). A `View` is the record with
//! what the server derives for it, as its reads and writes answer it; a
//! `Detail` adds the reader's abilities; a `Summary` is some of a View's keys
//! that another body embeds. A listing lists the View or its single read's body
//! where its rows are one, else a `ListRow`; a `{Parent}{Subject}Row` is a
//! child as its parent's routes draw it (`OrgUserRow`). An unpaged list of a
//! parent's children is `{Parent}{Subject}s`, and an item type of its own
//! `{Parent}{Subject}` (`PlanJobs`, `PlanJob`); a one-off read takes its
//! route's noun (`OrgOverview`, `PlanExplanation`). Never `Response`, `Record`,
//! `Params`, `Body` or `Api`, nor an `Account`, `Membership`, `Device` or
//! `Fleet` subject; `Answer` names only an untagged enum. The httpapi `openapi`
//! tests refuse the banned suffixes and subjects.
//!
//! A field is named for the value it carries, with one suffix per shape: `_id`
//! or `_ids`, `_count`, `_at`, `_by`, `_index`, and an enum after its type
//! (`document_state`). A tally such as `Counts` keys each count by what it
//! counts, and the paging keys `total`, `offset`, `limit` and `unreadable` keep
//! their names.

// Values.

mod text;
pub use text::{
    EmptyStringError, NonEmptyString, NonEmptyStringError, NonEmptyTrimmedString, TrimmedString,
};

mod numbers;
pub use numbers::{
    ElapsedMillis, ElapsedMillisError, NonNegativeCount, NonNegativeCountError, PositiveCount,
    PositiveCountError, PositiveCountRangeError, PositiveSeconds, PositiveSecondsError,
    SkinCelsius, SkinCelsiusError,
};

mod collections;
pub use collections::{NonEmptyVec, NonEmptyVecError};

mod labels;
pub use labels::{LabelError, Labels};

mod principal;
pub use principal::{
    ClientId, ContactEmail, ContactEmailError, OrgId, OrgSlug, OrgSlugError, PrincipalIdError,
    PublicKeyHex, PublicKeyHexError, Tag, TagError, UserId, UserKeyId, VaultPrincipalId,
    VaultResourceId,
};

mod permissions;
pub use permissions::{ClientPermission, PermissionName, PermissionNameError, UserPermission};

mod ids;
pub use ids::{
    AuditEventId, CampaignId, ClientPreauthKeyId, ExpiresAt, ExpiresAtError, IdError, JobId,
    ModelKey, PlanId, UserPreauthKeyId,
};

mod huggingface;
pub use huggingface::{
    AuthToken, AuthTokenError, HF_TOKEN_ENV, HfOrg, HfOrgError, HfRepo, HfRepoError, HfRepoName,
    HfRepoNameError, HfRevision, HfRevisionError, RepoSubpath, RepoSubpathError,
};

mod location;
pub use location::{
    AbsolutePath, AbsolutePathError, FileName, FileNameError, RelativePath, RelativePathError,
    RemoteArchiveUrl, RemoteArchiveUrlError, ResourceUrl, ResourceUrlError, Sha256, Sha256Error,
    VaultPath, VaultPathError,
};

mod descriptor;
pub use descriptor::{
    canonical_json, descriptor_digest, drop_vault_custom_labels, is_vault_source,
    names_vault_source, short_digest,
};

// The host a cell runs on.

mod device;
pub use device::{
    BatteryLevel, BatteryLevelError, CapabilityFlag, CapabilityFlagError, DeviceFormFactor,
    DeviceInfo,
};

mod thermal;
pub use thermal::{
    AndroidTemperatureSensor, AndroidThermalStatus, AndroidThrottlingSeverity, AppleThermalState,
    DevicePowerState, LinuxThermalZone, PowerReading, RunThermal, ThermalReading, ThermalTelemetry,
};

// What a cell is made of.

mod dataset;
pub use dataset::{
    CorpusFormat, Dataset, DatasetAdapter, DatasetPath, DatasetPathError, DatasetRepositoryUrl,
    DatasetRepositoryUrlError, DatasetUpstream, GitOid, GitOidError,
};

mod model;
pub use model::{
    GgufText, GgufTextSource, GgufVision, GgufVisionAbsoluteFiles, GgufVisionHfFiles,
    GgufVisionRelativeFiles, GgufVisionSource, GgufVisionSpeculative, GgufVisionSpeculativeSource,
    GgufVisionUrlFiles, GgufVisionVaultUrlFiles, Mlx, MlxSpeculative, Model, ModelSource,
    ModelType, OnnxGenai, Openvino, SpeculativePair, Torch, TorchSpeculative, inject_hf_auth_token,
};

mod runtime;
pub use runtime::{
    AppleFoundation, DockerIpcMode, DockerSglang, DockerShmSize, DockerShmSizeError, DockerVllm,
    EnvForward, EnvForwardError, LlamaCppFlavor, LlamacppApkPipette, LlamacppApkPipetteFlavor,
    LlamacppCliStockTools, LlamacppCliStockToolsRelease, LlamacppCliStockToolsSource,
    LlamacppFlashAttention, LlamacppIosPipette, LlamacppIosPipetteFlavor, LlamacppLoadMode,
    MlxIosPipette, MlxIosPipetteFlavor, MlxMacosPipette, MlxMacosPipetteFlavor, MlxSwiftStack,
    OnnxExecutionProvider, OnnxGenaiIosPipette, OnnxGenaiIosPipetteFlavor, OnnxGenaiStack,
    OpenvinoDevice, OpenvinoGenerateHint, Ovms, OvmsFlavor, OvmsSource, OvmsSourceRepository,
    PrivateApis, RepositoryUrl, Runtime, RuntimeType, SemverWithBuild, SemverWithBuildError,
    SglangFlavor, SourceRepository, TrtEdgellm, TrtEdgellmFlavor, TrtEdgellmPrecision,
    TrtEdgellmSource, UvBuild, UvBuildError, UvOnnxGenai, UvOpenvino, UvPythonVersion,
    UvPythonVersionError, UvRuntimeSource, UvServerVersion, UvServerVersionError, UvSglang, UvVllm,
    VllmDtype, VllmFlavor, VllmRepetitionDetection, cli_ref_from_runtime_arg, flavor_from_uv_build,
    uv_runtime_version,
};

mod descriptor_catalog;
mod descriptor_catalog_write;
pub use descriptor_catalog::{
    CatalogName, CatalogParseError, CatalogQualifier, CatalogScope, DescriptorDisplay,
    DescriptorEntry, DescriptorEntryCreator, DescriptorEntryKind, DescriptorEntryState,
    DescriptorKind, DescriptorMember, DescriptorMemberDescriptor, DescriptorMemberKey, EntryError,
    MemberError, ModelFormat, QualifiedCatalogName, QualifiedNameError, QuantizationLabel,
    QuantizationLabelError, hf_repos, hf_repos_mut,
};
pub use descriptor_catalog_write::{
    AddDescriptorMemberRequest, ChangeDescriptorEntryStateRequest, CreateDescriptorEntryRequest,
    DescriptorEntryAction, ModelMemberRequest,
};

mod vault_artifact;
pub use vault_artifact::{
    UploadVaultResourcesRequest, VaultArtifact, VaultEntry, VaultFileFormat, VaultFileRole,
    VaultModel, VaultRuntime, VaultStoredFile, VaultUploadFile,
};

mod chat;
pub use chat::{
    AudioFormat, ChatMessage, ChatRole, ContentPart, EvalSample, ImageUrl, InputAudio,
    MessageContent,
};

mod vlm_eval_corpus;
pub use vlm_eval_corpus::{
    SamplesDigest, VlmEvalCorpusContent, VlmEvalCorpusError, VlmEvalCorpusManifest,
    VlmEvalCorpusMessage, VlmEvalCorpusPart, VlmEvalCorpusSample, parse_samples_digest,
};

mod benchmark;
pub use benchmark::{
    AsrBeamSearch, AsrDatasetResult, AsrDecodeTokens, AsrDecodeTokensError, AsrDecoding,
    AsrDecodingError, AsrDecodingKnob, AsrErrorUnit, AsrEvalBenchmark, AsrEvalTask,
    AsrMaxMemoryBenchmark, AsrPromptProfile, AsrRepeatWindow, AsrSamplerChain, AsrSampling,
    AsrSuite, AsrSuiteHeadline, AsrThroughputBenchmark, AudioChannels, AudioChannelsError,
    AudioDurationMillis, AudioDurationMillisError, AudioSampleRate, AudioSampleRateError,
    BatchSize, BatchSizeError, BeamLengthPenalty, BeamLengthPenaltyError, BeamRepetitionPenalty,
    BeamRepetitionPenaltyError, BenchmarkDefinition, BenchmarkDefinitionError, BenchmarkEntry,
    BenchmarkId, BenchmarkIdError, BenchmarkParams, BenchmarkParamsError, BenchmarkSource,
    BenchmarkType, BenchmarkTypeError, DecodeThroughputBenchmark, EndToEndLatencyBenchmark,
    EvalBenchmark, EvalId, KnownAsrEvalTask, KnownAsrPromptProfile, KnownEvalId, KnownVlmEvalTask,
    KnownVlmPromptProfile, LanguageSubtag, LanguageSubtagError, MaxContextError,
    MaxMemoryUsageBenchmark, MinP, MinPError, NumBeams, NumBeamsError, PrefillThroughputBenchmark,
    PresencePenalty, PresencePenaltyError, RecipeRepo, RepeatPenalty, RepeatPenaltyError,
    SamplingSeed, SamplingSeedError, SamplingTemperature, SamplingTemperatureError, SeedRule,
    Thinking, TopK, TopKError, TopP, TopPError, VlMaxMemoryBenchmark, VlSpecDecodeBenchmark,
    VlThroughputBenchmark, VlmEvalBenchmark, VlmEvalRecipe, VlmEvalRecipeError, VlmEvalRecipeModel,
    VlmEvalTask, VlmPromptProfile,
};

// The schemas the `schemars` derive cannot write, and the flag enums' transform.

#[cfg(feature = "schemars")]
mod json_schema;

// The cell.

mod cell;
pub use cell::{
    AsrEvalLlamacppCliStockToolsGgufVision, AsrMaxMemoryLlamacppCliStockToolsGgufVision,
    AsrThroughputLlamacppCliStockToolsGgufVision, BenchmarkFlagRef, BenchmarkFlags,
    BenchmarkFlagsError, BenchmarkFlagsRecordError, ClientRunSpec,
    DecodeThroughputLlamacppCliStockToolsGgufText, DecodeThroughputOvmsOpenvino,
    DecodeThroughputUvOpenvinoOpenvino, EndToEndLatencyLlamacppCliStockToolsGgufText,
    EndToEndLatencyOvmsOpenvino, EndToEndLatencyUvOpenvinoOpenvino, EvalContextError,
    EvalLlamacppCliStockToolsGgufText, FlagsRecordError,
    MaxMemoryUsageLlamacppCliStockToolsGgufText, MaxMemoryUsageOvmsOpenvino,
    MaxMemoryUsageUvOpenvinoOpenvino, ModelFlagRef, ModelFlags, ModelFlagsError,
    ModelFlagsRecordError, PrefillThroughputLlamacppCliStockToolsGgufText,
    PrefillThroughputOvmsOpenvino, PrefillThroughputUvOpenvinoOpenvino, ReadinessOverrides,
    ReasoningEffort, RunSpecError, RuntimeFlagRef, RuntimeFlags, RuntimeFlagsError,
    TrtEdgellmGgufVision, VariantError, VlMaxMemoryLlamacppCliStockToolsGgufVision,
    VlSpecDecodeLlamacppCliStockTools, VlThroughputLlamacppCliStockToolsGgufVision,
    VlmEvalLlamacppCliStockToolsGgufVision, benchmark_type_of, is_compatible,
    resolve_benchmark_flags, resolve_benchmarks, resolve_model_flags, resolve_runtime_flags,
    resolve_speculation, runnable_pairs_of, runnable_type_pairs_of, validate_variant_flag_cells,
};

// What becomes of a cell.

mod plan;
pub use plan::{
    Matrix, Plan, PlanError, RetryConfig, RunnableCell, ShellType, TransportConfig, TypedCell,
    Variant,
};

mod extra_runtime_configuration;
pub use extra_runtime_configuration::{
    EngineExtraRuntimeConfiguration, ExtraRuntimeConfiguration, ExtraRuntimeConfigurationError,
    ExtraRuntimeConfigurationRef, ExtraRuntimeConfigurations, ExtraRuntimeConfigurationsError,
    LlamacppCliStockToolsExtraRuntimeConfiguration, MlxMacosPipetteExtraRuntimeConfiguration,
    OvmsExtraRuntimeConfiguration, TorchOaiExtraRuntimeConfiguration,
    TrtEdgellmExtraRuntimeConfiguration, UvOnnxGenaiExtraRuntimeConfiguration,
    UvOpenvinoExtraRuntimeConfiguration,
};

mod submission;
pub use submission::{
    AsrAudioTokensSource, AsrClientExtras, AsrClientExtrasError, AsrContextSizing, BatchItem,
    BatchSubmitted, BenchmarkEvalCompletion, BenchmarkEvalCompletionStopReason,
    BenchmarkResultData, BenchmarkSubmissionPayload, ByteCount, ByteCountError,
    ByteCountRangeError, ELLIPSIS, FailureInput, JobAccepted, MAX_CLIENT_EXTRAS_BYTES,
    MAX_DEVICE_READINGS_BYTES, MAX_READINESS_GATE_WAITS_BYTES, MAX_RUNTIME_OUTPUT_BYTES,
    MemoryObservation, NonNegativeMillis, NonNegativeMillisError, PositiveMillis,
    PositiveMillisError, RuntimeOutput, SampleCompletion, Submission, SubmissionInput,
    SuccessInput, keep_last_elements, keep_tail,
};

mod run;
pub use run::{DeclaredBound, RunRequest, RunRequestError, RunResponse, TurnObservation};

// The HTTP API's bodies: the device protocol and `/api/v1`, one module per
// route resource.

mod body_values;
pub use body_values::{
    ErrorBody, FleetDetail, ListOrder, comma_list, empty_as_none, parsed, parsed_list,
};

mod list;
pub use list::{Changed, ListPage, Named};

mod health;
pub use health::HealthResponse;

mod owners;
pub use owners::{
    ClientOwner, OwnedListQuery, OwnerFilter, OwnerLookupQuery, OwnerMatch, OwnerMatches,
    OwnerType, PrincipalOwner,
};

mod catalog;
pub use catalog::{CatalogEntry, CatalogQuery, OrgBenchmarks};

mod descriptor_catalog_view;
pub use descriptor_catalog_view::{
    DescriptorCatalogQuery, DescriptorCatalogShortList, DescriptorCatalogShortListItem,
    DescriptorDigestNames, DescriptorDigestQuery, DescriptorEntryListRow, DescriptorEntryLookup,
    DescriptorMemberCoverage, DescriptorMemberParameters, DescriptorMemberSummary,
    DescriptorMemberView, DescriptorNameHit, DescriptorNameQuery, DescriptorWithheldMember,
    DescriptorWithheldReasonCode, DescriptorWithheldReport, MemberSelector, RequestedCatalogName,
    SelectorError,
};

mod catalog_import_run;
pub use catalog_import_run::{
    CatalogImportCommand, CatalogImportOutcome, CatalogImportResult, CatalogImportRun,
    CatalogImportRuns, CatalogImportRunsQuery, unclassified_summary,
};

mod results;
pub use results::{FailureSource, OrUnknown, ResultListView, ResultRow, UnknownTag, UnknownValue};

mod jobs;
pub use jobs::{
    EvalSampleResult, ExpiredJob, FailedJob, Job, JobInPlan, JobMetric, JobQueue, JobRead,
    JobResult, JobState, JobStatus, Lease, ParentJob, ParentRun, PlanCell, PlanJob, ResultsRead,
    SubmittedJob,
};

mod users;
pub use users::{
    AddUserKeyRequest, AmendUserPermissionsRequest, CreateUserRequest, FirstClerkSignIn,
    MintUserPreauthRequest, OrgUserRow, OwnUserView, Reach, RegisterClerkUserKeyRequest,
    RegisterUserKeyRequest, SetUserOrgRequest, SetUserPermissionsRequest, UpdateUserRequest, User,
    UserAbilities, UserAccountType, UserDetail, UserKey, UserKeyRegistered, UserPermissionSet,
    UserPermissionsAmended, UserPreauthKey, UserPreauthKeyMinted, UserPreauthKeyView,
    UserPreauthUsage, UserStatus, UserView,
};

mod plans;
pub use plans::{
    Answered, CancelOutcome, ClaimAnswer, ClaimedJob, Counts, DocumentState, ImportAction,
    IngestEvent, IssueSeverity, JobExpiry, JobPlans, PlanAnswer, PlanCancelRequested, PlanDryRun,
    PlanFiled, PlanIssue, PlanJobs, PlanJobsQuery, PlanListQuery, PlanReadQuery, PlanStatus,
    PlanSummary, PlanValidation, PlanVariants, PlanView, PlannedImport, ProgressSnapshot,
    RequestedScope, RestoredVariant, RestoredVariantError, RevisionChange, StarvedClauses,
    StarvedGroup, UndecodableClaim, UpdatePlanRequest, Warning, WarningKind, unmatched_warning,
};

mod campaigns;
pub use campaigns::{
    Campaign, CampaignAnswer, CampaignCreated, CampaignDocument, CampaignListQuery,
    CampaignOverview, CampaignOverviewQuery, CampaignPlanDocumentQuery, CampaignPlanRow,
    CampaignPlans, CampaignReadQuery, CampaignRevised, CampaignState, CampaignVariants,
    CampaignView, CreateCampaignQuery, DeviceTimeTotal, HeldJob, RecalculateJobsRequest,
    RerunJobsQuery, RerunJobsRequest, RevertCampaignQuery, RevertCampaignRequest,
    ReviseCampaignQuery, Revised, RevisionAnswer, RevisionBase, RevisionDryRun, RevisionPreview,
    SetCampaignOwnerRequest, UpdateCampaignRequest,
};

mod clients;
pub use clients::{
    AmendClientPermissionsRequest, Backlog, BacklogByCampaign, BacklogCampaign,
    CapabilityFlagCount, CapabilityNamespace, CapabilitySource, CapabilityVocabulary,
    CapabilityVocabularyQuery, Client, ClientApproved, ClientDetail, ClientListQuery,
    ClientListRow, ClientPermissionSet, ClientPermissions, ClientPreauthKey, ClientPrivateKey,
    ClientRecord, ClientRegistered, ClientStatus, ClientView, DeviceProfile, DeviceProfileFields,
    FleetOwner, Refused, RegisterClient, SetClientOwnerRequest, SetClientPermissionsRequest,
    UpdateClient, UpdateClientRequest,
};

mod model_params;
pub use model_params::{CreateModelParamsRequest, ModelParams, UpdateModelParamsRequest};

mod orgs;
pub use orgs::{
    AssignClientsRequest, AssignedClient, ClientPreauthKeyMinted, ClientPreauthKeyView,
    ClientPreauthUsage, ClientsAssigned, CreateOrgRequest, Highlights, MintClientPreauthRequest,
    OrgListQuery, OrgOverview, OrgStatus, OrgView, Organization, UpdateOrgRequest,
};

mod explain;
pub use explain::{
    FleetStanding, JobCensus, NarrowingRung, PlanExplanation, RosterStanding, SchedulingState,
    StarvedGroupExplanation,
};

mod vault;
pub use vault::{
    GithubOwner, GithubOwnerError, GithubRepoName, GithubRepoNameError, ModelParamsDetail,
    ObjectState, VaultBlobStored, VaultEntryWithToml, VaultOrigin, VaultReadQuery, VaultResource,
    VaultResourceDetail, VaultResourcesUploaded, VaultType, VaultTypeFile, VaultTypes,
};

mod audit;
pub use audit::{
    AuditAction, AuditActor, AuditActorKind, AuditEvent, AuditOutcome, AuditQuery, AuditTarget,
};

#[cfg(feature = "schemars")]
mod app_schema;
#[cfg(feature = "schemars")]
pub use app_schema::{
    app_requests, app_responses, org_benchmark_schema, upload_vault_resources_request_schema,
};
