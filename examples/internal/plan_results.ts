// Download a plan's results CSV, signed by tools/sign.ts. No dependencies: Node >= 22.18, bun or deno.
//
//   node examples/internal/plan_results.ts [plan_id]      writes <plan_id>-results.csv
//
// PIPETTE_KEY picks the key as tools/sign.ts reads it; default the dashboard service account's.
import { writeFileSync } from "node:fs";
import { signedFetch, signerFromEnv } from "../../tools/sign.ts";

const DASHBOARD_KEY = "op://Pipette/32co4kiyioas75l3r7smwccgmm/private key";

const plan = process.argv[2] ?? "plan-01a12eb9-699e-716e-a48b-f0459a130f06";
const url = `https://collector.pipette.liquid.ai/api/v1/plans/${plan}/results.csv`;
const res = await signedFetch(await signerFromEnv(DASHBOARD_KEY), url);
const body = await res.text();
if (!res.ok) {
  console.error(`${res.status} ${body}`);
  process.exit(1);
}
writeFileSync(`${plan}-results.csv`, body);
console.log(`wrote ${plan}-results.csv`);
