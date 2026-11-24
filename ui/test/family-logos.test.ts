import { beforeEach, describe, expect, test, vi } from "vitest";

const descriptorFamilyLogo = vi.fn();
const session = {
  identity: { key_id: "first" },
  get signer() { return this.identity; },
};
vi.mock("../src/lib/api", () => ({ descriptorFamilyLogo }));
vi.mock("../src/lib/session.svelte", () => ({ session }));

const { familyLogo } = await import("../src/lib/family-logos");

describe("family logos", () => {
  beforeEach(() => {
    session.identity = { key_id: "first" };
    descriptorFamilyLogo.mockReset();
  });

  test("concurrent reads share a request and a later read checks access again", async () => {
    const sha256 = "a".repeat(64);
    descriptorFamilyLogo.mockResolvedValueOnce({ sha256, logo_uri: "data:image/png;base64,abcd" })
      .mockRejectedValueOnce(new Error("not found"));
    expect(await Promise.all([familyLogo(sha256), familyLogo(sha256)]))
      .toEqual(["data:image/png;base64,abcd", "data:image/png;base64,abcd"]);
    expect(descriptorFamilyLogo).toHaveBeenCalledTimes(1);
    expect(await familyLogo(sha256)).toBeNull();
    expect(descriptorFamilyLogo).toHaveBeenCalledTimes(2);
  });

  test("a failed read gives no logo and is tried again", async () => {
    const sha256 = "b".repeat(64);
    descriptorFamilyLogo.mockRejectedValueOnce(new Error("offline"))
      .mockResolvedValueOnce({ sha256, logo_uri: "data:image/png;base64,abcd" });
    expect(await familyLogo(sha256)).toBeNull();
    expect(await familyLogo(sha256)).toBe("data:image/png;base64,abcd");
    expect(descriptorFamilyLogo).toHaveBeenCalledTimes(2);
  });

  test("a new signer starts a fresh read and discards the previous signer's answer", async () => {
    const sha256 = "c".repeat(64);
    descriptorFamilyLogo.mockResolvedValueOnce({ sha256, logo_uri: "data:image/png;base64,abcd" })
      .mockRejectedValueOnce(new Error("not found"));
    const first = session.signer;
    const before = familyLogo(sha256);
    session.identity = { key_id: "second" };
    const after = familyLogo(sha256);

    expect(descriptorFamilyLogo).toHaveBeenNthCalledWith(1, first, sha256);
    expect(descriptorFamilyLogo).toHaveBeenNthCalledWith(2, session.signer, sha256);
    expect(await Promise.all([before, after])).toEqual([null, null]);
  });
});
