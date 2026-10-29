// The vault upload, the classic console's `console-vault-upload.js`:
// each file hashed here and streamed to `PUT /api/v1/vault/blobs/…`, then the files
// described to `POST /api/v1/vault/resources` (`docs/api/vault/upload_resources.md`).

import { describeUpload, putSigned, signedRaw, type Answer, type Signer } from "./api";
import { describe } from "./components/guard";
import { sha256Of } from "./sha256";
import type {
  OwnUserView, UploadVaultResourcesRequest, VaultFileFormat, VaultFileRole, VaultResourcesUploaded, VaultType, VaultTypes,
} from "./api-types";

/** `FileName`'s rule, checked before any byte is sent rather than after. */
export const FILE_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,254}$/;

// A `429` means every upload slot is busy; mgmt frees one as an upload ends.
// It is answered before the body is read, so a browser may see the connection
// drop instead, and a dropped upload is retried the same way.
export const BUSY_RETRY_MS = 10_000;
export const BUSY_RETRIES = 60;

export interface Owner {
  value: string;
  label: string;
}

/**
 * The owners the form offers: the reader, and the organization it belongs to,
 * which is `VaultAuthority`'s `gives_to`: membership alone.
 */
export function uploadOwners(account: OwnUserView): Owner[] {
  const owners = [{ value: account.user_id, label: "me" }];
  if (account.org_id) {
    owners.push({ value: account.org_id, label: `my organization (${account.org_id})` });
  }
  return owners;
}

/** What a file field takes, as its caption says it. */
export const takes = (format: VaultFileFormat) => (format === "gguf" ? "a .gguf" : "a .tar, .tar.gz or .zip");

/** The file picker's filter for a field. */
export const accept = (format: VaultFileFormat) => (format === "gguf" ? ".gguf" : ".tar,.gz,.tgz,.zip");

export const percent = (done: number, total: number) => `${total ? Math.floor((100 * done) / total) : 100}%`;

/** Why the chosen files cannot be sent as named, or `null` when they can. */
export function misnamed(files: File[]): string | null {
  const wrong = files.filter((file) => !FILE_NAME.test(file.name));
  if (wrong.length === 0) return null;
  return `Rename ${wrong.map((file) => file.name).join(", ")}: a file name is ` +
    "letters, digits, '.', '_' and '-', starting with a letter or digit.";
}

/** A type the form offers, and whether it is listed among the runtimes, which take a flavor. */
export interface Offered {
  vaultType: VaultType;
  runtime: boolean;
}

/** The type named `type`, from whichever list holds it. */
export function offered(types: VaultTypes, type: string): Offered | null {
  const named = (list: VaultType[]) => list.find((vaultType) => vaultType.type === type);
  const model = named(types.models);
  if (model) return { vaultType: model, runtime: false };
  const runtime = named(types.runtimes);
  return runtime ? { vaultType: runtime, runtime: true } : null;
}

/** The fields of the form, as typed. */
export interface UploadForm extends Offered {
  name: string;
  /** The owner chosen, an account or organization id. */
  owner: string;
  /** The reader, whom the body leaves unnamed. */
  me: string;
  flavor: string;
}

/** One chosen file, by the role it fills. */
export interface Chosen {
  role: VaultFileRole;
  file: File;
}

/** The description: `owner` left out for the reader, `flavor` for a model. */
export function uploadBody(form: UploadForm,
                           files: { role: VaultFileRole; filename: string; sha256: string }[]): UploadVaultResourcesRequest {
  const body: UploadVaultResourcesRequest = { name: form.name.trim(), type: form.vaultType.type };
  if (form.owner !== form.me) body.owner = form.owner;
  if (form.runtime) body.flavor = form.flavor.trim();
  for (const { role, filename, sha256 } of files) body[role] = { filename, sha256 };
  return body;
}

/** The calls an upload makes, so a test can stand in for the server and the clock. */
export interface Transport {
  /** Aborted when the page goes, which stops the upload where it is. */
  signal: AbortSignal;
  head(path: string): Promise<Answer>;
  put(path: string, file: Blob, progress: (sent: number) => void): Promise<Answer | null>;
  sleep(ms: number): Promise<void>;
  describe(body: UploadVaultResourcesRequest): Promise<VaultResourcesUploaded>;
}

/** `ms` of waiting, cut short by a rejection when `signal` aborts. */
export const pause = (ms: number, signal: AbortSignal) => new Promise<void>((resolve, reject) => {
  if (signal.aborted) return reject(signal.reason);
  const stop = () => {
    clearTimeout(timer);
    reject(signal.reason);
  };
  const timer = setTimeout(() => {
    signal.removeEventListener("abort", stop);
    resolve();
  }, ms);
  signal.addEventListener("abort", stop, { once: true });
});

/** The calls, each signed by `signer` as it is made, so a retry signs afresh. */
export const signedTransport = (signer: Signer, signal: AbortSignal): Transport => ({
  signal,
  head: (path) => signedRaw(signer, "HEAD", path, { signal }),
  put: (path, file, progress) => putSigned(signer, path, file, progress, signal),
  sleep: (ms) => pause(ms, signal),
  describe: (body) => describeUpload(signer, body),
});

const refused = (what: string, answer: Answer) => new Error(`${what}: ${answer.status}: ${answer.message}`);

/** Upload `file` for `principal` unless it is there already, and answer its digest. */
export async function uploadFile(transport: Transport, principal: string, file: File,
                                 say: (line: string) => void): Promise<string> {
  say(`${file.name}: hashing`);
  const sha256 = await sha256Of(file, (done) => say(`${file.name}: hashing ${percent(done, file.size)}`),
                                transport.signal);
  const path = `/api/v1/vault/blobs/${encodeURIComponent(principal)}/${sha256}`;
  const head = await transport.head(path);
  if (head.status === 200) {
    say(`${file.name}: already uploaded`);
    return sha256;
  }
  if (head.status !== 404) throw refused(file.name, head);
  for (let attempt = 1; ; attempt++) {
    transport.signal.throwIfAborted();
    const sent = await transport.put(path, file, (done) => say(`${file.name}: uploading ${percent(done, file.size)}`));
    if (sent && sent.status >= 200 && sent.status < 300) {
      say(`${file.name}: uploaded`);
      return sha256;
    }
    if (sent && sent.status !== 429) throw refused(file.name, sent);
    if (attempt === BUSY_RETRIES) throw sent ? refused(file.name, sent) : new Error(`${file.name}: the upload broke off`);
    say(sent ? `${file.name}: mgmt is busy with other uploads, retrying` : `${file.name}: the upload broke off, retrying`);
    await transport.sleep(BUSY_RETRY_MS);
  }
}

/**
 * Upload each chosen file, one at a time since mgmt bounds uploads in flight
 * across every caller, then describe them; answers the first record's id.
 * `say(i, line)` rewrites file `i`'s progress line.
 */
export async function upload(transport: Transport, form: UploadForm, chosen: Chosen[],
                             say: (index: number, line: string) => void): Promise<string> {
  const wrong = misnamed(chosen.map(({ file }) => file));
  if (wrong) throw new Error(wrong);
  const files = [];
  for (const [index, { role, file }] of chosen.entries()) {
    const sha256 = await uploadFile(transport, form.owner, file, (line) => say(index, line));
    files.push({ role, filename: file.name, sha256 });
  }
  transport.signal.throwIfAborted();
  let answer: VaultResourcesUploaded;
  try {
    answer = await transport.describe(uploadBody(form, files));
  } catch (e) {
    throw new Error(`describing the upload: ${describe(e)}`);
  }
  const first = answer.resources[0];
  if (!first) throw new Error("describing the upload: the answer names no resource");
  return first.resource_id;
}
