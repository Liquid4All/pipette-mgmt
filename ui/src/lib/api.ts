// Calls to the JSON API, each signed in the browser with RFC 9421
// (`docs/authentication.md` §2.4). No cookie: the API takes signatures only.

import { sha256, signRequest } from "./httpsig";
import { hex, unhex, type SigningKey } from "./keys";
import { everyPage } from "./table";
import type {
  AddDescriptorMemberRequest, AddUserKeyRequest, AmendClientPermissionsRequest, AmendUserPermissionsRequest, AnyValueIngestEvent, CampaignCreated, CampaignDocument, CampaignRevised, CatalogImportRuns,
  CampaignVariants,
  ChangedModelParams, ChangedOrgView, ClientDetail, ClientListRowPage, ClientPreauthKeyMinted, ClientPreauthKeyViewPage, CreateDescriptorEntryRequest, CreateModelParamsRequest, CreateOrgRequest, CreateUserRequest,
  DescriptorCatalogShortList, DescriptorDigestNames, DescriptorEntryListRow, DescriptorEntryListRowPage, DescriptorEntryLookup, MintClientPreauthRequest, MintUserPreauthRequest,
  ModelParams, ModelParamsPage,
  NamedAuditEventPage, NamedCampaignOverview, NamedCampaignView, NamedCampaignViewPage, NamedPlanJobs, NamedPlanView, NamedPlanViewPage, NamedVaultResourceDetail, NamedVaultResourceDetailPage, OrgBenchmarks,
  OrgOverview, OrgView, OrgViewPage, OwnUserView, OwnerMatches, OwnerType, PlanCancelRequested, PlanJob,
  PlanValidation, PlanVariants, RecalculateJobsRequest, RegisterClerkUserKeyRequest, RegisterUserKeyRequest, RerunJobsRequest, RevertCampaignRequest, RevisionDryRun, SetCampaignOwnerRequest,
  SetClientOwnerRequest, SetUserOrgRequest, UpdateCampaignRequest, UpdateModelParamsRequest, UpdateOrgRequest, UpdatePlanRequest, UpdateUserRequest,
  UploadVaultResourcesRequest, UserDetail, UserKeyPage, UserKeyRegistered, UserPreauthKeyMinted, UserPreauthKeyViewPage, UserView, UserViewPage, VaultResourcesUploaded,
  VaultTypes,
} from "./api-types";

export class ApiError extends Error {
  constructor(readonly status: number, readonly reason: string) {
    super(`${status}: ${reason}`);
  }
}

export interface Signer extends SigningKey {
  /** The account the key signs as, `null` until it is known; the signature names only the key. */
  user_id: string | null;
}

/** The authority the page came from, which the browser sends as `Host`. */
const pageAuthority = () => globalThis.location?.host ?? "";

/** The signature fields for a request; `digest` is the body's SHA-256, `null` for no body. */
export async function signedHeaders(signer: Signer, method: string, path: string,
                                    digest: Uint8Array | null = null, now = new Date(),
                                    authority = pageAuthority()): Promise<Record<string, string>> {
  const nonce = hex(crypto.getRandomValues(new Uint8Array(16)));
  return signRequest(signer.privateKey, signer.key_id, method, authority, path, digest,
                     Math.floor(now.getTime() / 1000), nonce);
}

/** `body`'s bytes and their SHA-256, or nothing for no body. */
async function digested(body: string | undefined): Promise<Uint8Array | null> {
  return body === undefined ? null : sha256(new TextEncoder().encode(body));
}

/** `text` as JSON, or `null` where it is none. */
function parsed(text: string): unknown {
  try { return text ? JSON.parse(text) : null; } catch { return null; }
}

/** What a refusal says: the envelope's `error`, else its text, else the status line. */
const refusal = (text: string, statusText: string) =>
  (parsed(text) as { error?: string } | null)?.error ?? (text || statusText);

async function send<T>(path: string, init: RequestInit): Promise<T> {
  const resp = await fetch(path, init);
  const text = await resp.text();
  if (!resp.ok) throw new ApiError(resp.status, refusal(text, resp.statusText));
  return parsed(text) as T;
}

let unauthorized: (signer: Signer) => void = () => {};

/** Told of every signed call the server refused with a `401`, and which key signed it. */
export function onUnauthorized(listener: (signer: Signer) => void) {
  unauthorized = listener;
}

async function signed<T>(signer: Signer, method: string, path: string, json?: unknown): Promise<T> {
  const body = json === undefined ? undefined : JSON.stringify(json);
  const headers = await signedHeaders(signer, method, path, await digested(body));
  if (body !== undefined) headers["Content-Type"] = "application/json";
  try {
    return await send<T>(path, { method, headers, body });
  } catch (e) {
    if (e instanceof ApiError && e.status === 401) unauthorized(signer);
    throw e;
  }
}

/** An answer the caller judges by its status, with what a refusal said. */
export interface Answer {
  status: number;
  message: string;
}

/** The answer to a request sent outside JSON: the status, the headers, and the body as text. */
export interface RawAnswer extends Answer {
  headers: Headers;
  text: string;
}

/**
 * A signed request whose status the caller reads, such as a `HEAD`, or whose
 * body is not JSON either way. `headers` go beside the signature's.
 */
export async function signedRaw(signer: Signer, method: string, path: string,
                                init: { headers?: Record<string, string>; body?: string; signal?: AbortSignal } = {},
): Promise<RawAnswer> {
  const headers = { ...init.headers, ...await signedHeaders(signer, method, path, await digested(init.body)) };
  const resp = await fetch(path, { method, headers, body: init.body, signal: init.signal });
  if (resp.status === 401) unauthorized(signer);
  const text = await resp.text();
  return { status: resp.status, message: refusal(text, resp.statusText), headers: resp.headers, text };
}

/** `answer`'s text, or the refusal it carries thrown as the JSON calls throw one. */
function accepted(answer: RawAnswer): string {
  if (answer.status < 200 || answer.status >= 300) throw new ApiError(answer.status, answer.message);
  return answer.text;
}

/**
 * `PUT` `body` at `path` as it streams from disk, reporting the bytes sent;
 * `null` when the connection drops, a rejection when `signal` stops it. XHR,
 * since fetch reports no upload progress. A vault path ends in the file's
 * SHA-256, which is its `Content-Digest`, so the file never sits in memory.
 */
export async function putSigned(signer: Signer, path: string, body: Blob, progress: (sent: number) => void,
                                signal?: AbortSignal): Promise<Answer | null> {
  const headers = await signedHeaders(signer, "PUT", path, unhex(path.slice(path.lastIndexOf("/") + 1)));
  signal?.throwIfAborted();
  const answer = await new Promise<Answer | null>((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const abort = () => xhr.abort();
    signal?.addEventListener("abort", abort, { once: true });
    xhr.onloadend = () => signal?.removeEventListener("abort", abort);
    xhr.onabort = () => reject(signal?.reason);
    xhr.open("PUT", path);
    for (const [name, value] of Object.entries(headers)) xhr.setRequestHeader(name, value);
    xhr.setRequestHeader("Content-Type", "application/octet-stream");
    xhr.upload.onprogress = (event) => progress(event.loaded);
    xhr.onload = () => resolve({ status: xhr.status, message: refusal(xhr.responseText, xhr.statusText) });
    xhr.onerror = () => resolve(null);
    xhr.send(body);
  });
  if (answer?.status === 401) unauthorized(signer);
  return answer;
}

/**
 * Register this browser's public key to the account a Clerk token names. The
 * server sets the key to expire, since the token proved a sign-in, not a machine.
 */
export const registerWithClerk = (token: string, body: RegisterClerkUserKeyRequest) =>
  send<UserKeyRegistered>("/api/v1/users/register/clerk", {
    method: "POST",
    headers: { Authorization: `Bearer ${token}`, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

/** Unsigned: the pre-auth secret is the credential. */
export const register = (body: RegisterUserKeyRequest) =>
  send<UserKeyRegistered>("/api/v1/users/register", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });

export const me = (signer: Signer) => signed<OwnUserView>(signer, "GET", "/api/v1/users/me");

/** The account `key` signs for, read with a signature that names the key alone. */
export const whoami = (key: SigningKey) => me({ ...key, user_id: null });

export const addKey = (signer: Signer, body: AddUserKeyRequest) =>
  signed<UserKeyRegistered>(signer, "POST", "/api/v1/users/me/keys", body);

export const revokeKey = (signer: Signer, key_id: string) =>
  signed<unknown>(signer, "DELETE", `/api/v1/users/me/keys/${encodeURIComponent(key_id)}`);

/** The organizations in reach; `counts: false` skips the member and device counts, which read every relation. */
export const orgs = async (signer: Signer, { counts = true }: { counts?: boolean } = {}) =>
  (await signed<OrgViewPage>(signer, "GET", counts ? "/api/v1/orgs" : "/api/v1/orgs?counts=false")).items;

/** The path of one organization, by handle or id, or of a route under it. */
const orgPath = (reference: string, rest = "") => `/api/v1/orgs/${encodeURIComponent(reference)}${rest}`;

/** One organization, by handle or id. */
export const org = (signer: Signer, reference: string) => signed<OrgView>(signer, "GET", orgPath(reference));

/** One organization's overview: its counts, what waits on a decision, its newest campaigns. */
export const orgOverview = (signer: Signer, reference: string) =>
  signed<OrgOverview>(signer, "GET", orgPath(reference, "/overview"));

export const createOrg = (signer: Signer, body: CreateOrgRequest) =>
  signed<OrgView>(signer, "POST", "/api/v1/orgs", body);

/** A mutation names the organization by id only; a handle is a `400`. */
export const updateOrg = (signer: Signer, org_id: string, body: UpdateOrgRequest) =>
  signed<ChangedOrgView>(signer, "PATCH", orgPath(org_id), body);

/**
 * The live catalog names of each of `digests` (at most 100) that the reader can read, ranked. A digest with no name is
 * absent.
 */
export const descriptorNamesByDigest = (signer: Signer, digests: readonly string[]) =>
  signed<DescriptorDigestNames>(signer, "GET", `/api/v1/descriptor-catalog/by-digest?digests=${digests.join(",")}`);

/** One page of global entries, with compact member summaries. */
export const globalCatalog = (signer: Signer, query: string) =>
  signed<DescriptorEntryListRowPage>(signer, "GET", `/api/v1/descriptor-catalog?${query}`);

/** Recent import and refresh runs, with the last successful import time. */
export const catalogImportRuns = (signer: Signer) =>
  signed<CatalogImportRuns>(signer, "GET", "/api/v1/descriptor-catalog/import-runs");

/** One global entry with its full members; fleet readers can request offer counts. */
export const globalCatalogEntry = (signer: Signer, name: string, offerCounts: boolean) => {
  const query = new URLSearchParams({ names: `global/${name}` });
  if (offerCounts) query.set("offer_counts", "true");
  return signed<DescriptorEntryLookup>(signer, "GET", `/api/v1/descriptor-catalog/names?${query}`);
};

/** The pinned and active global entries that offer a member to the device `client_id`, and the withheld members. */
export const deviceCatalog = (signer: Signer, client_id: string) =>
  signed<DescriptorCatalogShortList>(signer, "GET",
    `/api/v1/descriptor-catalog/devices/${encodeURIComponent(client_id)}`);

/** Creates a hand-made catalog entry in the organization `org_id`. The answer is the entry with its members. */
export const createOrgDescriptorEntry = (signer: Signer, org_id: string, body: CreateDescriptorEntryRequest) =>
  signed<DescriptorEntryListRow>(signer, "POST", orgPath(org_id, "/descriptor-catalog"), body);

/** Appends one member to the entry `name` of the organization `org_id`. The answer is the entry with its members. */
export const addOrgDescriptorMember = (signer: Signer, org_id: string, name: string,
                                       body: AddDescriptorMemberRequest) =>
  signed<DescriptorEntryListRow>(signer, "POST",
    orgPath(org_id, `/descriptor-catalog/${encodeURIComponent(name)}/members`), body);

/** Every live entry of the organization `org_id`, read page by page. With `members`, each row has the descriptors of
 * its members. */
export async function orgDescriptorEntries(signer: Signer, org_id: string,
                                           members = false): Promise<DescriptorEntryListRow[]> {
  const whole = await everyPage((cursor) => {
    const query = new URLSearchParams({
      scope: org_id, ...(members ? { members: "true" } : {}), ...(cursor ? { cursor } : {}),
    });
    return signed<DescriptorEntryListRowPage>(signer, "GET", `/api/v1/descriptor-catalog?${query}`);
  });
  return whole.items;
}

/** Every model-params row, in `{model_key}.json` byte order, with a count of the rows that did not read. */
export const modelParams = (signer: Signer) =>
  signed<ModelParamsPage>(signer, "GET", "/api/v1/model-params");

const modelParamsPath = (model_key: string) => `/api/v1/model-params/${encodeURIComponent(model_key)}`;

export const createModelParams = (signer: Signer, body: CreateModelParamsRequest) =>
  signed<ModelParams>(signer, "POST", "/api/v1/model-params", body);

export const updateModelParams = (signer: Signer, model_key: string, body: UpdateModelParamsRequest) =>
  signed<ChangedModelParams>(signer, "PUT", modelParamsPath(model_key), body);

export const deleteModelParams = (signer: Signer, model_key: string) =>
  signed<unknown>(signer, "DELETE", modelParamsPath(model_key));

export const users = async (signer: Signer) =>
  (await signed<UserViewPage>(signer, "GET", "/api/v1/users")).items;

/** The path of one account, or of a route under it. */
const user = (user_id: string, rest = "") => `/api/v1/users/${encodeURIComponent(user_id)}${rest}`;

/** One account the reader's roster reaches. */
export const account = (signer: Signer, user_id: string) =>
  signed<UserDetail>(signer, "GET", user(user_id));

export const createUser = (signer: Signer, body: CreateUserRequest) =>
  signed<UserView>(signer, "POST", "/api/v1/users", body);

export const updateUser = (signer: Signer, user_id: string, body: UpdateUserRequest) =>
  signed<unknown>(signer, "PATCH", user(user_id), body);

export const amendPermissions = (signer: Signer, user_id: string, body: AmendUserPermissionsRequest) =>
  signed<unknown>(signer, "PATCH", user(user_id, "/permissions"), body);

/** The answer carries no permissions: read the account again after it. */
export const setUserOrg = (signer: Signer, user_id: string, body: SetUserOrgRequest) =>
  signed<unknown>(signer, "PUT", user(user_id, "/org"), body);

export const userKeys = async (signer: Signer, user_id: string) =>
  (await signed<UserKeyPage>(signer, "GET", user(user_id, "/keys"))).items;

/** One of the account's keys, revoked by its administrator. */
export const revokeUserKey = (signer: Signer, user_id: string, key_id: string) =>
  signed<unknown>(signer, "DELETE", user(user_id, `/keys/${encodeURIComponent(key_id)}`));

/** The account's pre-auth keys, newest first. */
export const preauthKeys = async (signer: Signer, user_id: string) =>
  (await signed<UserPreauthKeyViewPage>(signer, "GET", user(user_id, "/preauth-keys"))).items;

/** The answer carries the token, the one time it is shown. */
export const mintPreauthKey = (signer: Signer, user_id: string, body: MintUserPreauthRequest) =>
  signed<UserPreauthKeyMinted>(signer, "POST", user(user_id, "/preauth-keys"), body);

export const revokePreauthKey = (signer: Signer, user_id: string, key_id: string) =>
  signed<unknown>(signer, "DELETE", user(user_id, `/preauth-keys/${encodeURIComponent(key_id)}`));

/** One page of an organization's audit log, newest first; `query` arrives encoded. */
export const orgAudit = (signer: Signer, org: string, query: string) =>
  signed<NamedAuditEventPage>(signer, "GET", `/api/v1/orgs/${encodeURIComponent(org)}/audit${query ? `?${query}` : ""}`);

/** One page of the whole audit log, newest first. */
export const audit = (signer: Signer, query: string) =>
  signed<NamedAuditEventPage>(signer, "GET", query ? `/api/v1/audit?${query}` : "/api/v1/audit");

/** One page of the fleet; `query` arrives encoded, since it is signed as sent. */
export const clients = (signer: Signer, query: string) =>
  signed<ClientListRowPage>(signer, "GET", query ? `/api/v1/clients?${query}` : "/api/v1/clients");

/** The path of one device, or of a route under it. */
const device = (client_id: string, rest = "") => `/api/v1/clients/${encodeURIComponent(client_id)}${rest}`;

/** One device, with what the reader may do to it. */
export const client = (signer: Signer, client_id: string) =>
  signed<ClientDetail>(signer, "GET", device(client_id));

export const setClientOwner = (signer: Signer, client_id: string, body: SetClientOwnerRequest) =>
  signed<unknown>(signer, "PUT", device(client_id, "/owner"), body);

/** Whom the device may be handed to whose handle, address or id holds `q`. */
export async function deviceOwners(signer: Signer, client_id: string, q: string, type?: OwnerType) {
  const query = new URLSearchParams(type ? { q, type } : { q }).toString();
  return (await signed<OwnerMatches>(signer, "GET", device(client_id, `/owners?${query}`))).owners;
}

export const approveClient = (signer: Signer, client_id: string) =>
  signed<unknown>(signer, "POST", device(client_id, "/approve"));

export const rejectClient = (signer: Signer, client_id: string) =>
  signed<unknown>(signer, "POST", device(client_id, "/reject"));

export const unsuspendClient = (signer: Signer, client_id: string) =>
  signed<unknown>(signer, "POST", device(client_id, "/unsuspend"));

export const deleteClient = (signer: Signer, client_id: string) =>
  signed<unknown>(signer, "DELETE", device(client_id));

export const amendClientPermissions = (signer: Signer, client_id: string, body: AmendClientPermissionsRequest) =>
  signed<unknown>(signer, "PATCH", device(client_id, "/permissions"), body);

/** The path of an organization's device pre-auth keys, or of one. */
const deviceKeys = (org_id: string, key_id = "") =>
  `/api/v1/orgs/${encodeURIComponent(org_id)}/client-preauth-keys${key_id ? `/${encodeURIComponent(key_id)}` : ""}`;

export const clientPreauthKeys = async (signer: Signer, org_id: string) =>
  (await signed<ClientPreauthKeyViewPage>(signer, "GET", deviceKeys(org_id))).items;

/** The answer carries the token, the one time it is shown. */
export const mintClientPreauthKey = (signer: Signer, org_id: string, body: MintClientPreauthRequest) =>
  signed<ClientPreauthKeyMinted>(signer, "POST", deviceKeys(org_id), body);

export const revokeClientPreauthKey = (signer: Signer, org_id: string, key_id: string) =>
  signed<unknown>(signer, "DELETE", deviceKeys(org_id, key_id));

/** The model and runtime types an upload takes, with their file fields. */
export const vaultTypes = (signer: Signer) =>
  signed<VaultTypes>(signer, "GET", "/api/v1/vault/types");

/** Describe uploaded files as the variant they make up, which writes one record per file. */
export const describeUpload = (signer: Signer, body: UploadVaultResourcesRequest) =>
  signed<VaultResourcesUploaded>(signer, "POST", "/api/v1/vault/resources", body);

/** One page of the vault; `query` arrives encoded, since it is signed as sent. */
export const vaultResources = (signer: Signer, query: string) =>
  signed<NamedVaultResourceDetailPage>(signer, "GET", `/api/v1/vault/resources?${query}`);

/** The path of one vault resource. */
const resource = (resource_id: string, rest = "") => `/api/v1/vault/resources/${encodeURIComponent(resource_id)}${rest}`;

/** One resource, with its owner's name and whether its file is still stored. */
export const vaultResource = (signer: Signer, resource_id: string) =>
  signed<NamedVaultResourceDetail>(signer, "GET", resource(resource_id, "?names=true&object=true"));

/** Deletes the resource and every other record of its upload. */
export const deleteVaultResource = (signer: Signer, resource_id: string) =>
  signed<unknown>(signer, "DELETE", resource(resource_id));

/** The path of an organization's private benchmarks, or of one. */
const catalog = (reference: string, benchmark_id = "") =>
  orgPath(reference, `/benchmarks${benchmark_id ? `/${encodeURIComponent(benchmark_id)}` : ""}`);

/** An organization's private benchmarks, with the ids whose entry does not read. */
export const orgBenchmarks = (signer: Signer, reference: string) =>
  signed<OrgBenchmarks>(signer, "GET", catalog(reference));

/** One entry as the catalog file holds it, which the forms edit. */
export const benchmarkToml = async (signer: Signer, reference: string, benchmark_id: string) =>
  accepted(await signedRaw(signer, "GET", catalog(reference, benchmark_id),
                           { headers: { Accept: "application/toml" } }));

/**
 * Write `toml` as the entry `benchmark_id`: `create` only where the
 * organization keeps none (`If-None-Match: *`), else only over one it keeps
 * (`If-Match: *`). Either refused is a `412`.
 */
export async function putBenchmark(signer: Signer, org_id: string, benchmark_id: string, toml: string,
                                   create: boolean): Promise<void> {
  const condition: Record<string, string> = create ? { "If-None-Match": "*" } : { "If-Match": "*" };
  accepted(await signedRaw(signer, "PUT", catalog(org_id, benchmark_id),
                           { headers: { "Content-Type": "application/toml", ...condition }, body: toml }));
}

export const deleteBenchmark = (signer: Signer, org_id: string, benchmark_id: string) =>
  signed<unknown>(signer, "DELETE", catalog(org_id, benchmark_id));

/** One page of the plans the reader reads; `query` arrives encoded, since it is signed as sent. */
export const plans = (signer: Signer, query: string) =>
  signed<NamedPlanViewPage>(signer, "GET", query ? `/api/v1/plans?${query}` : "/api/v1/plans");

/** The path of a plan route: the plan itself, or `rest` under it. */
const planPath = (plan_id: string, rest = "", query = "") =>
  `/api/v1/plans/${encodeURIComponent(plan_id)}${rest}${query ? `?${query}` : ""}`;

/** One plan, its submitter named. */
export const plan = (signer: Signer, plan_id: string) =>
  signed<NamedPlanView>(signer, "GET", planPath(plan_id, "", "names=true"));

/** `T` with the keys `K` required and non-null. */
type Present<T, K extends keyof T> = Omit<T, K> & { [P in K]-?: NonNullable<T[P]> };

/** A job of {@link PlanJobsWithResults}, which `results=true` always gives its `result`. */
export type PlanJobWithResult = Present<PlanJob, "result">;

/** The jobs body as `results=true` answers it: every key the flag adds is there. */
export type PlanJobsWithResults = Present<Omit<NamedPlanJobs, "jobs">, "results_read" | "may_cancel" | "cancel_permission" | "may_edit_note" | "document_state">
  & { jobs: PlanJobWithResult[] };

/**
 * A page of a plan's jobs with each job's result, the cancel offered and the
 * document's state; `query` carries `offset` and `limit`, or `all=true`.
 */
export const planJobsWithResults = (signer: Signer, plan_id: string, query = "") =>
  signed<PlanJobsWithResults>(signer, "GET", planPath(plan_id, "/jobs", query ? `results=true&${query}` : "results=true"));

/** The document a plan was filed from, as it was filed. */
export const planDocument = async (signer: Signer, plan_id: string) =>
  accepted(await signedRaw(signer, "GET", planPath(plan_id, "/document")));

/** A plan's `[[variants]]`, each holding the plan's benchmark list, for another document to append. */
export const planVariants = async (signer: Signer, plan_id: string) =>
  accepted(await signedRaw(signer, "GET", planPath(plan_id, "/variants")));

/** A plan's variants, one standalone block each, in the order that a variant edit's `restore` numbers them. */
export const planVariantList = async (signer: Signer, plan_id: string) =>
  parsed(accepted(await signedRaw(signer, "GET", planPath(plan_id, "/variants"),
                                  { headers: { Accept: "application/json" } }))) as PlanVariants;

/** Asks for the plan's cancel; the answer says what the request did, and the plan is read again. */
export const cancelPlan = (signer: Signer, plan_id: string) =>
  signed<PlanCancelRequested>(signer, "POST", planPath(plan_id, "/cancel"));

export const updatePlan = (signer: Signer, plan_id: string, body: UpdatePlanRequest) =>
  signed<unknown>(signer, "PATCH", planPath(plan_id), body);

/** Every result row of a plan as the CSV file the server writes, with the `Content-Disposition` that names it. */
export async function exportResults(signer: Signer, plan_id: string): Promise<{ csv: string; disposition: string | null }> {
  const answer = await signedRaw(signer, "GET", planPath(plan_id, "/results.csv"));
  return { csv: accepted(answer), disposition: answer.headers.get("Content-Disposition") };
}

/** One page of the campaigns the reader reads; `query` arrives encoded, since it is signed as sent. */
export const campaigns = (signer: Signer, query: string) =>
  signed<NamedCampaignViewPage>(signer, "GET", `/api/v1/campaigns?${query}`);

/** Send `toml` to `path` with `method`, `POST` by default; the query is signed with it. */
const postToml = async <T>(signer: Signer, path: string, toml: string, method = "POST", signal?: AbortSignal) =>
  parsed(accepted(await signedRaw(signer, method, path,
                                  { headers: { "Content-Type": "application/toml" }, body: toml, signal }))) as T;

/** One line of the progress stream that a route answers when it files a plan (`docs/api/campaigns.md`, "Filing a plan document"). */
export type IngestLine = AnyValueIngestEvent;

/** A `progress` line: the bytes of one file that the import downloaded so far. */
export type IngestProgress = Extract<IngestLine, { event: "progress" }>;

/** Why a stream with no last line fails: the server can still complete the write after the connection closes. */
export const STREAM_CUT =
  "The connection closed before the server answered. The server can still complete the change. " +
  "Read the campaign before you file again.";

/** Each line of `body` as text, without its newline. A line can arrive across two or more chunks. */
async function* lines(body: ReadableStream<Uint8Array>): AsyncGenerator<string> {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let rest = "";
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    // `stream: true` keeps a character split across two chunks for the next one.
    const parts = (rest + decoder.decode(value, { stream: true })).split("\n");
    rest = parts.pop() ?? "";
    yield* parts;
  }
  rest += decoder.decode();
  if (rest) yield rest;
}

/**
 * Send `toml` to `path` as `postToml` does, and read the answer as the
 * progress stream. `progress` gets each `progress` line. The promise resolves
 * with the body of the `result` line, or with the plain body of a server that
 * does not stream. It rejects with an `ApiError` for the `error` line or for a
 * refusal before the stream. It rejects with `STREAM_CUT` for a stream with no
 * last line, which includes a stream that breaks or ends in part of a line. It
 * also rejects with `STREAM_CUT` for a connection that closes before the answer.
 */
async function streamToml<T>(signer: Signer, path: string, toml: string, method: string,
                             progress: (line: IngestProgress) => void, signal?: AbortSignal): Promise<T> {
  const headers = {
    Accept: "application/x-ndjson", "Content-Type": "application/toml",
    ...await signedHeaders(signer, method, path, await digested(toml)),
  };
  try {
    const resp = await fetch(path, { method, headers, body: toml, signal });
    if (resp.status === 401) unauthorized(signer);
    if (!resp.ok) {
      const text = await resp.text();
      throw new ApiError(resp.status, refusal(text, resp.statusText));
    }
    if (!resp.body) throw new Error(STREAM_CUT);
    // A server that does not stream answers with the plain body, which holds the same answer.
    if (!resp.headers.get("Content-Type")?.startsWith("application/x-ndjson")) return parsed(await resp.text()) as T;
    for await (const text of lines(resp.body)) {
      if (!text.trim()) continue;
      const line = JSON.parse(text) as IngestLine;
      // A `keepalive` line or an unknown event has nothing to show.
      if (line.event === "progress") progress(line);
      else if (line.event === "result") return line.body as T;
      else if (line.event === "error") throw new ApiError(line.status, line.error);
    }
  } catch (e) {
    // A refusal and the `error` line are the server's answer, and an abort is the reader's.
    // Each other failure is a broken connection or a line cut short.
    if (e instanceof ApiError || signal?.aborted) throw e;
    throw new Error(STREAM_CUT, { cause: e });
  }
  throw new Error(STREAM_CUT);
}

/** `POST /api/v1/plans/validate`: what filing `toml` would refuse or warn about, and how many jobs it expands to. */
export const validatePlan = (signer: Signer, toml: string, signal?: AbortSignal) =>
  postToml<PlanValidation>(signer, "/api/v1/plans/validate", toml, "POST", signal);

/**
 * `POST /api/v1/campaigns` with `toml` as the first plan's document; `query`
 * names the campaign. Creates the campaign with its first plan, and reports
 * each file that its import downloads to `progress`.
 */
export const createCampaign = (signer: Signer, query: string, toml: string,
                               progress: (line: IngestProgress) => void, signal?: AbortSignal) =>
  streamToml<CampaignCreated>(signer, `/api/v1/campaigns?${query}`, toml, "POST", progress, signal);

/** The path of one campaign, or of a route under it. */
const campaignPath = (campaign_id: string, rest = "") => `/api/v1/campaigns/${encodeURIComponent(campaign_id)}${rest}`;

/** One campaign's record. */
export const campaign = (signer: Signer, campaign_id: string) =>
  signed<NamedCampaignView>(signer, "GET", campaignPath(campaign_id));

/**
 * One campaign, what the reader may do to it, and the plans it has run. Set
 * `deviceTime` to also read the device-time totals. Each total costs one query
 * to pipette-duckdb.
 */
export const campaignOverview = (signer: Signer, campaign_id: string, deviceTime = false) =>
  signed<NamedCampaignOverview>(signer, "GET", campaignPath(campaign_id, deviceTime ? "/overview?device_time=true" : "/overview"));

export const updateCampaign = (signer: Signer, campaign_id: string, body: UpdateCampaignRequest) =>
  signed<unknown>(signer, "PATCH", campaignPath(campaign_id), body);

export const setCampaignOwner = (signer: Signer, campaign_id: string, body: SetCampaignOwnerRequest) =>
  signed<unknown>(signer, "PUT", campaignPath(campaign_id, "/owner"), body);

/** Whom the campaign may be handed to whose handle, address or id holds `q`. */
export async function campaignOwners(signer: Signer, campaign_id: string, q: string) {
  const query = new URLSearchParams({ q }).toString();
  return (await signed<OwnerMatches>(signer, "GET", campaignPath(campaign_id, `/owners?${query}`))).owners;
}

/** The plan a document revision replaces, and the document it was filed from. */
export const campaignDocument = (signer: Signer, campaign_id: string) =>
  signed<CampaignDocument>(signer, "GET", campaignPath(campaign_id, "/document"));

/**
 * Files `toml` in place of the document `query`'s base was filed from; the
 * same document files nothing. Reports each file that the import downloads to
 * `progress`.
 */
export const reviseDocument = (signer: Signer, campaign_id: string, query: string, toml: string,
                               progress: (line: IngestProgress) => void, signal?: AbortSignal) =>
  streamToml<unknown>(signer, campaignPath(campaign_id, `/document?${query}`), toml, "PUT", progress, signal);

/** What the document revision would file; nothing is written. */
export const previewDocument = (signer: Signer, campaign_id: string, query: string, toml: string) =>
  postToml<RevisionDryRun>(signer, campaignPath(campaign_id, `/document?${query}&dry_run=true`), toml, "PUT");

/** The variants of the plan that the campaign runs, in the order that a variant edit's `remove` numbers them. */
export const campaignVariants = (signer: Signer, campaign_id: string) =>
  signed<CampaignVariants>(signer, "GET", campaignPath(campaign_id, "/variants"));

/** The server previews the variant edit in `toml` and `query`. It writes nothing. */
export const previewVariantEdit = (signer: Signer, campaign_id: string, query: string, toml: string) =>
  postToml<RevisionDryRun>(signer, campaignPath(campaign_id, `/variants?${query}&dry_run=true`), toml);

/** Files the variant edit `toml` with `query`; the answer names the revision that the campaign now runs. */
export const editVariants = (signer: Signer, campaign_id: string, query: string, toml: string) =>
  postToml<CampaignRevised>(signer, campaignPath(campaign_id, `/variants?${query}`), toml);

/** The query of a re-run, recalculation or revert read off `base`, which a moved campaign refuses. */
const baseQuery = (base: string) => new URLSearchParams({ base_plan_id: base }).toString();

/** Re-runs jobs of the plan the campaign runs; the answer names the revision it now runs. */
export const rerunJobs = (signer: Signer, campaign_id: string, base: string, body: RerunJobsRequest) =>
  signed<CampaignRevised>(signer, "POST", campaignPath(campaign_id, `/rerun?${baseQuery(base)}`), body);

/** Previews a revert of the campaign from `base` to the earlier plan in `body`. The call writes nothing. */
export const previewRevert = (signer: Signer, campaign_id: string, base: string, body: RevertCampaignRequest) =>
  signed<RevisionDryRun>(signer, "POST", campaignPath(campaign_id, `/revert?${baseQuery(base)}&dry_run=true`), body);

/** Reverts the campaign from `base` to the earlier plan in `body`. If that plan has the document of `base`, the call files nothing. */
export const revertCampaign = (signer: Signer, campaign_id: string, base: string, body: RevertCampaignRequest) =>
  signed<unknown>(signer, "POST", campaignPath(campaign_id, `/revert?${baseQuery(base)}`), body);

/** Recalculates finished evals of the plan the campaign runs; the answer names the revision. */
export const recalculateJobs = (signer: Signer, campaign_id: string, base: string, body: RecalculateJobsRequest) =>
  signed<CampaignRevised>(signer, "POST", campaignPath(campaign_id, `/recalculate?${baseQuery(base)}`), body);
