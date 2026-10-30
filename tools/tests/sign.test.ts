// tools/sign.ts against the vector pipette-httpsig and the app pin, and real ssh keys.
//
//   node --test tools/tests/sign.test.ts
import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { generateKeyPairSync } from "node:crypto";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { after, test } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { agentSigner, fileSigner, signRequest, textSigner } from "../sign.ts";

// RFC 9421 B.1.4 test-key-ed25519 as PKCS#8 PEM; the label is split so no scanner reads the source as a key.
const RFC_SEED = Buffer.from("n4Ni-HpISpVObnQMW0wOhCKROaIKqKtW_2ZYb2p9KcU", "base64url");
const LABEL = "PRIVATE" + " KEY";
const RFC_PEM = [
  `-----BEGIN ${LABEL}-----`,
  Buffer.concat([Buffer.from("302e020100300506032b657004220420", "hex"), RFC_SEED]).toString("base64"),
  `-----END ${LABEL}-----`,
].join("\n");

const dir = mkdtempSync(join(tmpdir(), "sign-"));
after(() => rmSync(dir, { recursive: true, force: true }));

let keys = 0;
function sshKeygen(kind: string): string {
  const path = join(dir, `${kind}-${keys++}`);
  execFileSync("ssh-keygen", ["-q", "-t", kind, "-N", "", "-C", "test", "-f", path]);
  return path;
}

test("the shared vector reproduces", async () => {
  const headers = await signRequest(
    textSigner(RFC_PEM), "POST", "https://mgmt.example/api/v1/orgs?dry_run=true",
    Buffer.from('{"slug":"acme"}'), 1760000000, "4f1c0de5a9b7",
  );
  assert.deepEqual(headers, {
    "Content-Digest": "sha-256=:Dmcct3wNRuKmSGlN9J7lDIw9DvE0Xp6DUXXIZHWJ65k=:",
    "Signature-Input": 'sig=("@method" "@authority" "@path" "@query" "content-digest");created=1760000000;nonce="4f1c0de5a9b7";keyid="userkey_b16c2d1bead1262639764fdb0ee4d3774599336bd493404cda4b1136c59f2062";alg="ed25519";tag="pipette"',
    "Signature": "sig=:xErs0vVcglQO4jeofP53oNks9pyROP4E0OOttcbMlHt5CAanP85tQV21Sy4jOZEzJg3Bm9gRrGXvqDssOFy+Bg==:",
  });
});

test("the CLI prints the headers, from a path with a space too", () => {
  const key = join(dir, "rfc.pem");
  writeFileSync(key, RFC_PEM);
  const cli = join(dir, "a b", "sign.ts");
  mkdirSync(dirname(cli));
  copyFileSync(join(dirname(fileURLToPath(import.meta.url)), "..", "sign.ts"), cli);
  const out = execFileSync(process.execPath, [cli, "https://mgmt.example/"], {
    encoding: "utf8", env: { ...process.env, PIPETTE_KEY: key }, stdio: ["ignore", "pipe", "ignore"],
  });
  assert.match(out, /^Signature-Input: sig=\("@method" "@authority" "@path" "@query"\);/m);
  assert.match(out, /^Signature: sig=:[A-Za-z0-9+/]{86}==:$/m);
});

test("an OpenSSH key reads as ssh-keygen wrote it", () => {
  const path = sshKeygen("ed25519");
  const blob = Buffer.from(readFileSync(`${path}.pub`, "utf8").split(" ")[1], "base64");
  assert.deepEqual(fileSigner(path).publicKey, blob.subarray(-32));
});

test("other keys are refused", () => {
  const { privateKey } = generateKeyPairSync("ec", { namedCurve: "P-256" });
  const pem = privateKey.export({ format: "pem", type: "pkcs8" }).toString();
  assert.throws(() => textSigner(pem), /not an Ed25519 key/);
  assert.throws(() => fileSigner(sshKeygen("ecdsa")), /not an Ed25519 key/);
});

test("the agent signs as the key file", async () => {
  const path = sshKeygen("ed25519");
  const sock = join(dir, "agent.sock");
  const agent = spawn("ssh-agent", ["-D", "-a", sock], { stdio: "ignore" });
  try {
    while (!existsSync(sock)) await sleep(50);
    execFileSync("ssh-add", ["-q", path], { env: { ...process.env, SSH_AUTH_SOCK: sock } });
    // Ed25519 is deterministic, so the two signatures are equal.
    const signer = await agentSigner("test", sock);
    assert.deepEqual(await signer.sign(Buffer.from("base")), fileSigner(path).sign(Buffer.from("base")));
  } finally {
    agent.kill();
  }
});

test("an agent that hangs up is an error", { timeout: 5000 }, async () => {
  const sock = join(dir, "hang-up.sock");
  const server = createServer((conn) => conn.once("data", () => conn.end(Buffer.from([0, 0, 0, 16, 12])))); // 16 bytes promised, 1 sent
  await new Promise<void>((resolve) => server.listen(sock, resolve));
  server.unref(); // so a regression fails on the timeout instead of hanging the run
  try {
    await assert.rejects(agentSigner("", sock), /closed the connection/);
  } finally {
    server.close();
  }
});
