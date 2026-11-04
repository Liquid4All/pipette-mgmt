import { afterEach, describe, expect, test, vi } from "vitest";
import {
  ApiError, STREAM_CUT, addOrgDescriptorMember, catalogEntries, catalogEntry, changeOrgDescriptorEntryState,
  createCampaign, createModelParams, deleteModelParams, deviceCatalog, jobPlans, onUnauthorized, orgDescriptorEntries, orgs,
  relabelOrgDescriptorMember, renameOrgDescriptorEntry, revokeUserKey, updateModelParams,
  type IngestProgress, type Signer,
} from "../src/lib/api";
import { generateKey } from "../src/lib/keys";

afterEach(() => {
  vi.unstubAllGlobals();
  onUnauthorized(() => {});
});

const answering = (status: number) =>
  vi.fn(async () => new Response(JSON.stringify({ error: "refused" }), { status }));

describe("the organization listing", () => {
  test("leaves the counts out only when asked", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = vi.fn(async () => new Response(JSON.stringify({ items: [], next_cursor: null, unreadable: 0 }), { status: 200 }));
    vi.stubGlobal("fetch", fetched);

    await orgs(signer);
    await orgs(signer, { counts: false });
    expect(fetched.mock.calls.map((call: unknown[]) => call[0])).toEqual(["/api/v1/orgs", "/api/v1/orgs?counts=false"]);
  });
});

describe("a model-params row", () => {
  test("a create names its model_key, and an edit and a delete address the row by it", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_admin" };
    const sent: { path: string; init: RequestInit }[] = [];
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      sent.push({ path, init });
      return new Response(null, { status: 204 });
    }));

    await createModelParams(signer, { model_key: "LiquidAI/AcmeLM-4.5B-Text", total_millions: 4512 });
    await updateModelParams(signer, "vault_9", { total_millions: 4512 });
    await deleteModelParams(signer, "vault_9");
    expect(sent.map(({ path, init }) => `${init.method} ${path}`)).toEqual([
      "POST /api/v1/model-params", "PUT /api/v1/model-params/vault_9", "DELETE /api/v1/model-params/vault_9",
    ]);
    expect(JSON.parse(String(sent[0]?.init.body))).toEqual(
      { model_key: "LiquidAI/AcmeLM-4.5B-Text", total_millions: 4512 });
  });
});

describe("a job's plans", () => {
  test("a dot segment, which the URL parser would drop from the path, is refused as the route refuses a bad id, unsent", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = vi.fn(async () => new Response(JSON.stringify({ fleet_detail: "full", plans: [], body: null }), { status: 200 }));
    vi.stubGlobal("fetch", fetched);

    await jobPlans(signer, "job-1");
    await expect(jobPlans(signer, "..")).rejects.toThrow("400: job_id contains invalid character '.'");
    await expect(jobPlans(signer, ".")).rejects.toThrow("400: job_id contains invalid character '.'");
    expect(fetched.mock.calls.map((call: unknown[]) => call[0])).toEqual(["/api/v1/jobs/job-1/plans"]);
  });
});

describe("an org catalog entry", () => {
  test("the entries of an organization are read page by page, and a member is added at the entry's path", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_kyle" };
    const pages = [
      { items: [{ name: "acme-lm" }], next_cursor: "org_acme/acme-lm", unreadable: 0 },
      { items: [{ name: "my-fork" }], next_cursor: null, unreadable: 0 },
      { items: [{ name: "my-build" }], next_cursor: null, unreadable: 0 },
    ];
    const sent: { path: string; init: RequestInit }[] = [];
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      sent.push({ path, init });
      const page = init.method === "GET" ? pages.shift() : { name: "acme lm" };
      return new Response(JSON.stringify(page), { status: 200 });
    }));

    const entries = await orgDescriptorEntries(signer, "org_acme");
    const withMembers = await orgDescriptorEntries(signer, "org_acme", true);
    await addOrgDescriptorMember(signer, "org_acme", "acme lm", { quantization: "q8_0" });
    expect(entries.map((entry) => entry.name)).toEqual(["acme-lm", "my-fork"]);
    expect(withMembers.map((entry) => entry.name)).toEqual(["my-build"]);
    expect(sent.map(({ path, init }) => `${init.method} ${path}`)).toEqual([
      "GET /api/v1/descriptor-catalog?scope=org_acme",
      "GET /api/v1/descriptor-catalog?scope=org_acme&cursor=org_acme%2Facme-lm",
      "GET /api/v1/descriptor-catalog?scope=org_acme&members=true",
      "POST /api/v1/orgs/org_acme/descriptor-catalog/acme%20lm/members",
    ]);
    expect(JSON.parse(String(sent[3]?.init.body))).toEqual({ quantization: "q8_0" });
  });
});

describe("the catalog pages", () => {
  test("the list and name lookup use read routes, with counts only on the name lookup", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = vi.fn(async () => new Response(JSON.stringify({ items: [], entries: [], unknown: [],
      next_cursor: null, unreadable: 0 }), { status: 200 }));
    vi.stubGlobal("fetch", fetched);

    await catalogEntries(signer, "descriptor_kind=runtime&state=active%2Cpinned");
    await catalogEntry(signer, "My.Build-1", false);
    await catalogEntry(signer, "global/My.Build-1", true);
    expect(fetched.mock.calls.map((call: unknown[]) => call[0])).toEqual([
      "/api/v1/descriptor-catalog?descriptor_kind=runtime&state=active%2Cpinned",
      "/api/v1/descriptor-catalog/names?names=My.Build-1",
      "/api/v1/descriptor-catalog/names?names=global%2FMy.Build-1&offer_counts=true&evidence=true",
    ]);
  });

  test("the org entry writes post to the entry's routes with the stored name", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = vi.fn(async (path: string, _init: RequestInit) => path.endsWith("/rename")
      ? new Response(JSON.stringify({ name: "AcmeLM", display_name: "AcmeLM", scope: "org_acme" }),
          { status: 200, headers: { "content-type": "application/json" } })
      : new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetched);

    await changeOrgDescriptorEntryState(signer, "org_acme", "acme lm", { action: "tombstone", confirmation: "acme lm" });
    expect((await renameOrgDescriptorEntry(signer, "org_acme", "acme lm", { name: "AcmeLM" }))
      .display_name).toBe("AcmeLM");
    await relabelOrgDescriptorMember(signer, "org_acme", "acme lm", { format: "gguf", from: "f16_q4_0", to: "q4_0" });
    expect(fetched.mock.calls.map(([path, init]) => [`${init.method} ${path}`, JSON.parse(String(init.body))]))
      .toEqual([
        ["POST /api/v1/orgs/org_acme/descriptor-catalog/acme%20lm/state", { action: "tombstone", confirmation: "acme lm" }],
        ["POST /api/v1/orgs/org_acme/descriptor-catalog/acme%20lm/rename", { name: "AcmeLM" }],
        ["POST /api/v1/orgs/org_acme/descriptor-catalog/acme%20lm/members/relabel",
         { format: "gguf", from: "f16_q4_0", to: "q4_0" }],
      ]);
  });

  test("a device's short list reads the device route", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = vi.fn(async () => new Response(JSON.stringify({ items: [], next_cursor: null, unreadable: 0,
      withheld: { member_count: 0, entry_count: 0, members: [] } }), { status: 200 }));
    vi.stubGlobal("fetch", fetched);

    await deviceCatalog(signer, "ev1_device");
    expect(fetched.mock.calls.map((call: unknown[]) => call[0])).toEqual([
      "/api/v1/descriptor-catalog/devices/ev1_device",
    ]);
  });
});

describe("a refused signature", () => {
  test("is reported with the key that signed it", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const told: string[] = [];
    onUnauthorized((by) => told.push(by.key_id));

    vi.stubGlobal("fetch", answering(401));
    await expect(orgs(signer)).rejects.toBeInstanceOf(ApiError);
    expect(told).toEqual([key.key_id]);

    vi.stubGlobal("fetch", answering(403));
    await expect(orgs(signer)).rejects.toThrow("403: refused");
    expect(told).toEqual([key.key_id]);
  });
});

describe("an account's key", () => {
  test("an administrator revokes it through the account that holds it", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_admin" };
    const sent: { path: string; init: RequestInit }[] = [];
    vi.stubGlobal("fetch", vi.fn(async (path: string, init: RequestInit) => {
      sent.push({ path, init });
      return sent.length === 1
        ? new Response(null, { status: 204 })
        : new Response(JSON.stringify({ error: "no such key on this account" }), { status: 404 });
    }));

    await revokeUserKey(signer, "user_x", "userkey_abc");
    expect(sent.map(({ path, init }) => `${init.method} ${path}`)).toEqual(["DELETE /api/v1/users/user_x/keys/userkey_abc"]);
    expect((sent[0]?.init.headers as Record<string, string>)["Signature"]).toMatch(/^sig=:[A-Za-z0-9+/]+=*:$/);

    const refused = revokeUserKey(signer, "user_x", "userkey_abc");
    await expect(refused).rejects.toBeInstanceOf(ApiError);
    await expect(refused).rejects.toMatchObject({ status: 404, message: "404: no such key on this account" });
  });
});

describe("a streamed filing", () => {
  const progress = { event: "progress", hf_ref: "a/b@0123", file: "m.gguf", bytes_done: 5, bytes_total: 10 };
  const created = { campaign_id: "campaign-x" };
  /** A `200` stream whose body arrives as `chunks`. A `broken` stream then fails as a lost connection does. */
  const streaming = (chunks: string[], broken = false) => vi.fn(async (_path: string, _init: RequestInit) => new Response(new ReadableStream<Uint8Array>({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(new TextEncoder().encode(chunk));
      if (!broken) controller.close();
    },
    pull(controller) {
      if (broken) controller.error(new TypeError("network error"));
    },
  }), { status: 200, headers: { "Content-Type": "application/x-ndjson" } }));
  const ndjson = (...lines: unknown[]) => lines.map((line) => `${JSON.stringify(line)}\n`).join("");
  const result = { event: "result", status: 201, body: created };

  test.each([
    { name: "reports progress and resolves on the result",
      chunks: [ndjson(progress, { event: "keepalive" }, { event: "later" }, result)],
      progress: [progress], answer: created },
    { name: "joins a line split across chunks",
      chunks: [ndjson(progress).slice(0, 20), ndjson(progress).slice(20) + ndjson(result).slice(0, 7), ndjson(result).slice(7)],
      progress: [progress], answer: created },
    { name: "rejects on the error line with its status and message",
      chunks: [ndjson(progress, { event: "error", status: 400, error: "rejected job" })],
      progress: [progress], refused: { status: 400, message: "400: rejected job" } },
    { name: "rejects a stream with no last line",
      chunks: [ndjson(progress, { event: "keepalive" })],
      progress: [progress], cut: true },
    { name: "rejects a stream that ends in part of a line",
      chunks: [ndjson(progress) + ndjson(result).slice(0, 7)],
      progress: [progress], cut: true },
    { name: "rejects a stream whose connection breaks",
      chunks: [ndjson(progress)], broken: true,
      progress: [progress], cut: true },
  ])("$name", async ({ chunks, broken, progress: expected, answer, refused, cut }) => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const fetched = streaming(chunks, broken);
    vi.stubGlobal("fetch", fetched);
    const seen: IngestProgress[] = [];
    const gone = new AbortController();

    const filing = createCampaign(signer, "name=x", "[plan]", (line) => seen.push(line), gone.signal);
    if (answer) await expect(filing).resolves.toEqual(answer);
    if (refused) {
      await expect(filing).rejects.toBeInstanceOf(ApiError);
      await expect(filing).rejects.toMatchObject(refused);
    }
    if (cut) await expect(filing).rejects.toThrow(STREAM_CUT);
    expect(seen).toEqual(expected);
    const init = fetched.mock.calls[0]?.[1];
    expect((init?.headers as Record<string, string>)["Accept"]).toBe("application/x-ndjson");
    expect(init?.signal).toBe(gone.signal);
  });

  test("resolves on the plain body of a server that does not stream", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify(created),
                                                          { status: 201, headers: { "Content-Type": "application/json" } })));
    await expect(createCampaign(signer, "name=x", "[plan]", () => {})).resolves.toEqual(created);
  });

  test.each([
    { name: "rejects a connection that closes before the answer", aborted: false },
    { name: "rejects an abort before the answer with the abort", aborted: true },
  ])("$name", async ({ aborted }) => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    const gone = new AbortController();
    const abort = new DOMException("The operation was aborted.", "AbortError");
    vi.stubGlobal("fetch", vi.fn(async () => {
      if (aborted) gone.abort(abort);
      throw aborted ? abort : new TypeError("NetworkError when attempting to fetch resource.");
    }));
    const filing = createCampaign(signer, "name=x", "[plan]", () => {}, gone.signal);
    await expect(filing).rejects.toThrow(aborted ? abort.message : STREAM_CUT);
  });

  test("rejects a refusal before the stream as the plain calls do", async () => {
    const key = await generateKey();
    const signer: Signer = { key_id: key.key_id, privateKey: key.privateKey, user_id: "user_x" };
    vi.stubGlobal("fetch", answering(403));
    await expect(createCampaign(signer, "name=x", "[plan]", () => {})).rejects.toThrow("403: refused");
  });
});
