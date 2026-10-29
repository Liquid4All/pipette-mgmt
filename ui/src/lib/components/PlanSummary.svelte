<script lang="ts">
  // The Overview's summary of the results: what the checks found, each linked to the page or the Jobs filter that
  // lists it.
  import { e2eText, pairingText } from "../findings";
  import { cellsWith, linesOf, listedText } from "../jobs";
  import { E2E, MISSING, shortLabel, SPEC, SWAP, VL_E2E } from "../report";
  import { issueCounts, type IssueCount } from "../summary";
  import { tabLink, type TabBase } from "../tabs";
  import type { Results } from "../results.svelte";
  import type { PlanJobsWithResults } from "../api";

  let { base, results, jobs }: {
    base: TabBase;
    results: Results;
    /** Every job, in the same read as the results: the issues are counted over their lines. */
    jobs: PlanJobsWithResults;
  } = $props();

  const index = $derived(results.index);
  const found = $derived(issueCounts(linesOf(jobs.jobs)).filter((issue) => issue.lines));
  const listed = (issue: IssueCount) => (issue.key === "swap" && index.swapped ? tabLink(base, "analysis", SWAP)
    : issue.key === "params" && index.missing ? tabLink(base, "analysis", MISSING) : cellsWith(base, { issue: [issue.key] }));
</script>

<h2>Checks</h2>
<dl class="facts">
  {#if index.e2e}
    <div><dt>{shortLabel(E2E)}</dt>
      <dd>{e2eText(index.e2e)}
        {#if index.e2e.kind === "checked"}<a href={tabLink(base, "analysis", E2E)}>See the check</a>{/if}</dd></div>
  {/if}
  {#if index.vlE2e}
    <div><dt>{shortLabel(VL_E2E)}</dt>
      <dd>{e2eText(index.vlE2e)} <a href={tabLink(base, "analysis", VL_E2E)}>See the check</a></dd></div>
  {/if}
  {#if index.pairing}
    <div><dt>VL speculative pairs</dt>
      <dd>{pairingText(index.pairing)} <a href={tabLink(base, "analysis", SPEC)}>See the charts</a></dd></div>
  {/if}
  {#each found as issue (issue.key)}
    <div><dt>{issue.label}</dt><dd><a href={listed(issue)}>{listedText(issue.jobs, issue.lines, null)}</a></dd></div>
  {/each}
  {#if !found.length}<div><dt>Issues</dt><dd>none found</dd></div>{/if}
</dl>
