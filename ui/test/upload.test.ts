import { afterEach, describe, expect, test, vi } from "vitest";
import { onUnauthorized, putSigned, type Answer, type Signer } from "../src/lib/api";
import { contentDigest } from "../src/lib/httpsig";
import { generateKey, unhex } from "../src/lib/keys";
import { Sha256 } from "../src/lib/sha256";
import {
  BUSY_RETRIES, BUSY_RETRY_MS, misnamed, offered, pause, upload, uploadBody, uploadOwners, type Chosen, type Transport,
  type UploadForm,
} from "../src/lib/upload";
import type {
  OwnUserView, UploadVaultResourcesRequest, UserPermission, VaultFileRole, VaultResourcesUploaded, VaultType,
} from "../src/lib/api-types";
import { nonceOf, verifies } from "./verify";
import { abilitiesOf } from "./abilities";

const USER = "user_0199aaaa";
const ORG = "org_0199bbbb";

const account = (permissions: UserPermission[], org_id?: string): OwnUserView => ({
  user_id: USER, contact_email: "ops@acme.example", account_type: "human", status: "active", created_at: "2026-10-01T00:00:00Z",
  permissions, keys: [], may: abilitiesOf(permissions, org_id), ...(org_id ? { org_id } : {}),
});

const vaultType = (type: string, roles: VaultFileRole[], flavors: string[] = []): VaultType => ({
  type, description: "", flavors, files: roles.map((role) => ({ role, optional: false, format: "gguf" })),
});

const VISION = vaultType("gguf_vision", ["model", "mmproj"]);
const OVMS = vaultType("ovms", ["archive"], ["windows-x64"]);

const form = (overrides: Partial<UploadForm> = {}): UploadForm =>
  ({ name: " acme LFM2-VL-3B q4 ", vaultType: VISION, runtime: false, owner: USER, me: USER, flavor: "", ...overrides });

const digestOf = (text: string) => {
  const hash = new Sha256();
  hash.update(new TextEncoder().encode(text));
  return hash.hex();
};

const answered = (status: number, message = ""): Answer => ({ status, message });

/** A server for one upload: `heads` and `puts` answer in turn, every call is recorded. */
function fake(heads: Answer[], puts: (Answer | null)[], signal = new AbortController().signal) {
  const calls: string[] = [];
  const described: UploadVaultResourcesRequest[] = [];
  const transport: Transport = {
    signal,
    head: async (path) => { calls.push(`HEAD ${path}`); return heads.shift() ?? answered(404); },
    put: async (path, file, progress) => {
      calls.push(`PUT ${path}`);
      progress(file.size);
      return puts.length ? puts.shift()! : answered(201);
    },
    sleep: async (ms) => { calls.push(`sleep ${ms}`); },
    describe: async (body) => {
      described.push(body);
      return { entry: {}, toml: "{}", resources: [{ resource_id: "vault_1" }] } as unknown as VaultResourcesUploaded;
    },
  };
  return { transport, calls, described };
}

describe("uploadOwners", () => {
  test("offers the organization to every member of it, as VaultAuthority's gives_to", () => {
    expect(uploadOwners(account(["manage_orgs"]))).toEqual([{ value: USER, label: "me" }]);
    expect(uploadOwners(account([], ORG))).toEqual([
      { value: USER, label: "me" },
      { value: ORG, label: `my organization (${ORG})` },
    ]);
  });
});

describe("uploadBody", () => {
  const files = [{ role: "model" as const, filename: "m.gguf", sha256: "a".repeat(64) }];

  test("leaves the reader unnamed and trims the name", () => {
    expect(uploadBody(form(), files)).toEqual(
      { name: "acme LFM2-VL-3B q4", type: "gguf_vision", model: { filename: "m.gguf", sha256: "a".repeat(64) } });
  });

  test("names another owner, and a runtime's trimmed flavor", () => {
    const body = uploadBody(form({ owner: ORG, vaultType: OVMS, runtime: true, flavor: " windows-x64 " }),
      [{ role: "archive", filename: "ovms.zip", sha256: "b".repeat(64) }]);
    expect(body).toEqual({
      name: "acme LFM2-VL-3B q4", type: "ovms", owner: ORG, flavor: "windows-x64",
      archive: { filename: "ovms.zip", sha256: "b".repeat(64) },
    });
  });
});

describe("offered", () => {
  test("finds a type in either list, a runtime as one", () => {
    const types = { models: [VISION], runtimes: [OVMS] };
    expect(offered(types, "gguf_vision")).toEqual({ vaultType: VISION, runtime: false });
    expect(offered(types, "ovms")).toEqual({ vaultType: OVMS, runtime: true });
    expect(offered(types, "gguf")).toBeNull();
  });
});

describe("misnamed", () => {
  test("names every file FileName refuses", () => {
    const named = (name: string) => new File(["x"], name);
    expect(misnamed([named("LFM2-1.2B_Q4.gguf")])).toBeNull();
    expect(misnamed([named(".hidden"), named("ok.gguf"), named("a b.gguf")]))
      .toBe("Rename .hidden, a b.gguf: a file name is letters, digits, '.', '_' and '-', " +
        "starting with a letter or digit.");
  });
});

describe("upload", () => {
  const model = new File(["GGUF model"], "model.gguf");
  const mmproj = new File(["GGUF projector"], "mmproj.gguf");
  const chosen: Chosen[] = [{ role: "model", file: model }, { role: "mmproj", file: mmproj }];
  const blob = (text: string, principal = USER) => `/api/v1/vault/blobs/${principal}/${digestOf(text)}`;

  test("skips a file already uploaded, sends the rest in turn, then describes them", async () => {
    const { transport, calls, described } = fake([answered(200), answered(404)], []);
    const lines: string[] = [];
    const id = await upload(transport, form(), chosen, (index, line) => { lines[index] = line; });
    expect(id).toBe("vault_1");
    expect(calls).toEqual([
      `HEAD ${blob("GGUF model")}`,
      `HEAD ${blob("GGUF projector")}`,
      `PUT ${blob("GGUF projector")}`,
    ]);
    expect(lines).toEqual(["model.gguf: already uploaded", "mmproj.gguf: uploaded"]);
    expect(described).toEqual([{
      name: "acme LFM2-VL-3B q4", type: "gguf_vision",
      model: { filename: "model.gguf", sha256: digestOf("GGUF model") },
      mmproj: { filename: "mmproj.gguf", sha256: digestOf("GGUF projector") },
    }]);
  });

  test("uploads under the owner chosen", async () => {
    const { transport, calls, described } = fake([], []);
    await upload(transport, form({ owner: ORG }), chosen.slice(0, 1), () => {});
    expect(calls).toEqual([`HEAD ${blob("GGUF model", ORG)}`, `PUT ${blob("GGUF model", ORG)}`]);
    expect(described[0]?.owner).toBe(ORG);
  });

  test("waits and retries while mgmt is busy or the connection drops", async () => {
    const { transport, calls } = fake([], [answered(429), null, answered(201)]);
    const lines: string[] = [];
    await upload(transport, form(), chosen.slice(0, 1), (index, line) => { lines.push(line); });
    expect(calls).toEqual([
      `HEAD ${blob("GGUF model")}`,
      `PUT ${blob("GGUF model")}`, `sleep ${BUSY_RETRY_MS}`,
      `PUT ${blob("GGUF model")}`, `sleep ${BUSY_RETRY_MS}`,
      `PUT ${blob("GGUF model")}`,
    ]);
    expect(lines).toContain("model.gguf: mgmt is busy with other uploads, retrying");
    expect(lines).toContain("model.gguf: the upload broke off, retrying");
  });

  test("gives up after the last retry", async () => {
    const { transport, calls, described } = fake([], Array.from({ length: BUSY_RETRIES }, () => null));
    await expect(upload(transport, form(), chosen.slice(0, 1), () => {}))
      .rejects.toThrow("model.gguf: the upload broke off");
    expect(calls.filter((call) => call.startsWith("PUT"))).toHaveLength(BUSY_RETRIES);
    expect(calls.filter((call) => call.startsWith("sleep"))).toHaveLength(BUSY_RETRIES - 1);
    expect(described).toEqual([]);

    const busy = fake([], Array.from({ length: BUSY_RETRIES }, () => answered(429, "too many vault uploads")));
    await expect(upload(busy.transport, form(), chosen.slice(0, 1), () => {}))
      .rejects.toThrow("model.gguf: 429: too many vault uploads");
  });

  test("stops at a refusal, describing nothing", async () => {
    const refused = fake([], [answered(400, "the file is not a gguf")]);
    await expect(upload(refused.transport, form(), chosen, () => {}))
      .rejects.toThrow("model.gguf: 400: the file is not a gguf");
    expect(refused.calls.filter((call) => call.startsWith("sleep"))).toEqual([]);
    expect(refused.described).toEqual([]);

    const outside = fake([answered(403, "you may only upload to the vault for yourself")], []);
    await expect(upload(outside.transport, form(), chosen, () => {}))
      .rejects.toThrow("model.gguf: 403: you may only upload to the vault for yourself");
    expect(outside.calls).toHaveLength(1);
  });

  test("refuses a misnamed file before sending a byte", async () => {
    const { transport, calls } = fake([], []);
    await expect(upload(transport, form(), [{ role: "model", file: new File(["x"], "my model.gguf") }], () => {}))
      .rejects.toThrow("Rename my model.gguf");
    expect(calls).toEqual([]);
  });

  test("stops when the page goes, sending nothing more and describing nothing", async () => {
    const gone = new AbortController();
    const { transport, calls, described } = fake([], [answered(429)], gone.signal);
    transport.sleep = (ms) => {
      calls.push(`sleep ${ms}`);
      const slept = pause(ms, gone.signal);
      gone.abort();
      return slept;
    };
    await expect(upload(transport, form(), chosen, () => {})).rejects.toThrow("aborted");
    expect(calls).toEqual([`HEAD ${blob("GGUF model")}`, `PUT ${blob("GGUF model")}`, `sleep ${BUSY_RETRY_MS}`]);
    expect(described).toEqual([]);
  });

  test("says which step a refused description failed at", async () => {
    const { transport } = fake([], []);
    transport.describe = async () => { throw new Error("400: unknown type `gguf`"); };
    await expect(upload(transport, form(), chosen.slice(0, 1), () => {}))
      .rejects.toThrow("describing the upload: 400: unknown type `gguf`");
  });
});

/** The bits of `XMLHttpRequest` a `PUT` uses, answering each send with the next status. */
class FakeXhr {
  static statuses: (number | null)[] = [];
  static sent: { method: string; path: string; headers: Record<string, string>; body: unknown }[] = [];
  #method = "";
  #path = "";
  #headers: Record<string, string> = {};
  status = 0;
  statusText = "";
  responseText = "";
  upload: { onprogress: ((event: { loaded: number }) => void) | null } = { onprogress: null };
  onload: (() => void) | null = null;
  onerror: (() => void) | null = null;
  onabort: (() => void) | null = null;
  onloadend: (() => void) | null = null;
  #aborted = false;

  open(method: string, path: string) { [this.#method, this.#path] = [method, path]; }
  setRequestHeader(name: string, value: string) { this.#headers[name] = value; }
  abort() {
    this.#aborted = true;
    this.onabort?.();
    this.onloadend?.();
  }
  send(body: Blob) {
    FakeXhr.sent.push({ method: this.#method, path: this.#path, headers: this.#headers, body });
    const status = FakeXhr.statuses.length ? FakeXhr.statuses.shift() as number | null : 201;
    queueMicrotask(() => {
      this.upload.onprogress?.({ loaded: body.size });
      if (this.#aborted) return;
      if (status === null) this.onerror?.();
      else {
        this.status = status;
        this.responseText = status >= 400 ? JSON.stringify({ error: "refused" }) : "";
        this.onload?.();
      }
      this.onloadend?.();
    });
  }
}

describe("putSigned", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
    onUnauthorized(() => {});
    FakeXhr.sent = [];
    FakeXhr.statuses = [];
  });

  test("streams the file as it is, and signs each attempt afresh", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: USER };
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    FakeXhr.statuses = [null, 429, 201];
    const file = new File(["GGUF model"], "model.gguf");
    const path = `/api/v1/vault/blobs/${USER}/${digestOf("GGUF model")}`;
    const progress: number[] = [];

    const answers = [];
    for (let i = 0; i < 3; i++) answers.push(await putSigned(signer, path, file, (sent) => progress.push(sent)));

    expect(answers).toEqual([null, { status: 429, message: "refused" }, { status: 201, message: "" }]);
    expect(progress).toEqual([file.size, file.size, file.size]);
    for (const { method, path: sentTo, headers, body } of FakeXhr.sent) {
      expect([method, sentTo, body]).toEqual(["PUT", path, file]);
      expect(headers["Content-Type"]).toBe("application/octet-stream");
      expect(headers["Content-Digest"]).toBe(contentDigest(unhex(digestOf("GGUF model"))));
      expect(await verifies(key.publicKey, "PUT", path, headers)).toBe(true);
    }
    expect(new Set(FakeXhr.sent.map(({ headers }) => nonceOf(headers))).size).toBe(3);
  });

  test("stops a PUT in flight when the page goes", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: USER };
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    const gone = new AbortController();
    await expect(putSigned(signer, `/api/v1/vault/blobs/x/${"00".repeat(32)}`, new File(["x"], "x.gguf"), () => gone.abort(), gone.signal))
      .rejects.toThrow("aborted");
    expect(FakeXhr.sent).toHaveLength(1);
    await expect(putSigned(signer, `/api/v1/vault/blobs/x/${"00".repeat(32)}`, new File(["x"], "x.gguf"), () => {}, gone.signal))
      .rejects.toThrow("aborted");
    expect(FakeXhr.sent).toHaveLength(1);
  });

  test("reports a refused signature", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: USER };
    const told: string[] = [];
    onUnauthorized((by) => told.push(by.key_id));
    vi.stubGlobal("XMLHttpRequest", FakeXhr);
    FakeXhr.statuses = [401];
    await putSigned(signer, `/api/v1/vault/blobs/x/${"00".repeat(32)}`, new File(["x"], "x.gguf"), () => {});
    expect(told).toEqual([key.key_id]);
  });
});
