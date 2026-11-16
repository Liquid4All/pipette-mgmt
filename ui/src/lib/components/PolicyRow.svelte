<script lang="ts">
  // One check's policy on a campaign group's Policies tab. Turn on asks first, showing what the check alone would do
  // now with each member campaign, so it is offered only once the preview has loaded. Turn on and Turn off send the
  // stored re-runs and settings: the preview counted them, and the route keeps them on a Turn off, so a typed
  // value's problem never stops it. A check that only flags takes neither, so it is turned on and off alone.
  import { tick } from "svelte";
  import {
    FAILURE_CODES, MAX_RERUNS, codesProblem, maxReruns, minReruns, percentProblem, policyBody, policyText, policyTitle, rerunsOrMarks,
    rerunsProblem, shortProblem, turnOffText, turnOnText,
  } from "../campaign-groups";
  import type { CampaignGroupPolicy, FailureCode, SetCampaignGroupPolicyRequest } from "../api-types";
  import ConfirmDialog from "./ConfirmDialog.svelte";
  import { describe } from "./guard";
  import SettingRow, { type RowIds } from "./SettingRow.svelte";
  import StatusPill from "./StatusPill.svelte";
  import { Busy } from "./work.svelte";

  let { policy, save, counts }: {
    policy: CampaignGroupPolicy;
    /** Write the policy, then read the policies again; whether it changed. Absent for a reader who may not write. */
    save?: (body: SetCampaignGroupPolicyRequest) => Promise<boolean>;
    /**
     * What this check's policy alone would do now with each member campaign, a line each; absent until the preview
     * loads.
     */
    counts?: string[];
  } = $props();

  const uid = $props.id();
  const edits = $derived(!!save && rerunsOrMarks(policy.kind));
  // Kept apart, so a read that leaves a stored value as it was, after another row's write, keeps what was typed here.
  const storedReruns = $derived(String(policy.max_reruns));
  const storedPercent = $derived(String(policy.settings.percent ?? ""));
  const storedShortMs = $derived(String(policy.settings.short_repetition_ms ?? ""));
  const storedShortPercent = $derived(String(policy.settings.short_repetition_percent ?? ""));
  // Each code's re-runs, a string in FAILURE_CODES order, so a read of the same codes, in any order, keeps what was
  // typed here.
  const storedCodes = $derived(FAILURE_CODES.map((code) => policy.settings.codes?.[code] ?? "").join(" "));
  let max_reruns = $derived(storedReruns);
  let percent = $derived(storedPercent);
  let shortMs = $derived(storedShortMs);
  let shortPercent = $derived(storedShortPercent);
  let codes = $derived(storedCodes);
  const typed = $derived(Object.fromEntries(codes.split(" ").map((n, at) => [FAILURE_CODES[at], n])));
  const changed = $derived(max_reruns !== storedReruns || percent !== storedPercent || shortMs !== storedShortMs
    || shortPercent !== storedShortPercent || codes !== storedCodes);
  const problems = $derived({
    percent: percentProblem(policy.kind, percent), short: shortProblem(policy.kind, shortMs, shortPercent),
    codes: codesProblem(policy.kind, typed), reruns: rerunsProblem(policy.kind, max_reruns),
  });
  const problem = $derived(problems.percent || problems.short || problems.codes || problems.reruns);
  const writing = new Busy();
  /**
   * What the last write did, and whether it left the policy on, so a policy turned on again elsewhere stops saying it
   * was turned off.
   */
  let done = $state<{ said: string; enabled: boolean } | null>(null);
  let row = $state<ReturnType<typeof SettingRow>>();

  async function write(body: SetCampaignGroupPolicyRequest, said: string) {
    if (!save || writing.busy) return;
    done = null;
    await writing.attempt(async () => {
      done = { said: (await save(body)) ? said : "Nothing to change.", enabled: body.enabled };
      reset();
      // Turning a policy off removes its button, so the focus moves to what the write did, once it is drawn.
      if (!body.enabled) {
        await tick();
        row?.focusStatus();
      }
    });
  }

  function submit(event: SubmitEvent) {
    event.preventDefault();
    const fields = {
      max_reruns, percent, short_repetition_ms: shortMs, short_repetition_percent: shortPercent, codes: typed,
    };
    if (changed && !problem) void write(policyBody(policy, fields), "Saved.");
  }

  function reset() {
    max_reruns = storedReruns;
    percent = storedPercent;
    shortMs = storedShortMs;
    shortPercent = storedShortPercent;
    codes = storedCodes;
  }

  function type(code: FailureCode, n: string) {
    codes = FAILURE_CODES.map((each) => (each === code ? n : typed[each])).join(" ");
    done = null;
  }

  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  /** The lines the open dialog shows, taken as it opens with the body it sends, so a read under it changes neither. */
  let asked = $state<string[]>([]);

  function turnOn() {
    if (!save || !counts || writing.busy) return;
    const body = policyBody(policy, "on");
    asked = counts;
    void confirm?.ask({
      title: `Turn on the ${policyTitle(policy)} policy?`,
      text: turnOnText(policy),
      fields: lines,
      action: "Turn on",
      pending: "Turning on…",
      // A refusal stays in the dialog: a 403 names the grant, and a 409 has read the policies again.
      run: async () => {
        done = null;
        try {
          done = { said: (await save(body)) ? "Turned on." : "Nothing to change.", enabled: true };
        } catch (e) {
          return describe(e);
        }
        reset();
        return null;
      },
      // Turning a policy on removes its button, so the focus moves to what the write did, once it is drawn.
      done: () => void tick().then(() => row?.focusStatus()),
    });
  }
</script>

{#snippet lines()}
  {#if asked.length}
    <p>What it alone would do now with each campaign's answered jobs:</p>
    <ul>{#each asked as line}<li>{line}</li>{/each}</ul>
  {/if}
{/snippet}

{#snippet body(ids: RowIds)}
  {#if edits}
    <form novalidate onsubmit={submit} aria-labelledby={ids.title}>
      <div class="bar">
        {#if policy.kind === "spread"}
          <label><span>Threshold (%)</span>
            <input type="number" min="0" step="any" value={percent} aria-describedby="{ids.text} {uid}-note"
                   aria-invalid={!!problems.percent || undefined}
                   oninput={(e) => { percent = e.currentTarget.value; done = null; }}></label>
          <label><span>Repetitions under (ms)</span>
            <input type="number" min="0" step="any" value={shortMs} aria-describedby="{ids.text} {uid}-note"
                   aria-invalid={!!problems.short || undefined}
                   oninput={(e) => { shortMs = e.currentTarget.value; done = null; }}></label>
          <label><span>Their threshold (%)</span>
            <input type="number" min="0" step="any" value={shortPercent} aria-describedby="{ids.text} {uid}-note"
                   aria-invalid={!!problems.short || undefined}
                   oninput={(e) => { shortPercent = e.currentTarget.value; done = null; }}></label>
        {/if}
        {#if policy.kind === "failed"}
          <fieldset>
            <legend>Re-runs by failure code (empty or 0: not re-run)</legend>
            {#each FAILURE_CODES as code (code)}
              <label><span><code>{code}</code></span>
                <input type="number" min="0" max={MAX_RERUNS} step="1" value={typed[code]}
                       aria-describedby="{ids.text} {uid}-note" aria-invalid={!!problems.codes || undefined}
                       oninput={(e) => type(code, e.currentTarget.value)}></label>
            {/each}
          </fieldset>
        {:else}
          <label><span>Re-runs</span>
            <input type="number" min={minReruns(policy.kind)} max={maxReruns(policy.kind)} step="1" value={max_reruns}
                   aria-describedby="{ids.text} {uid}-note" aria-invalid={!!problems.reruns || undefined}
                   oninput={(e) => { max_reruns = e.currentTarget.value; done = null; }}></label>
        {/if}
        <button type="submit" class="primary" aria-disabled={writing.busy || !changed || !!problem}>
          Save changes</button>
        {#if changed}<button type="button" onclick={reset}>Reset</button>{/if}
      </div>
      <div id="{uid}-note" class="hint err" role="status" hidden={!problem}>{problem}</div>
    </form>
  {/if}
  {#if writing.error}<p class="err" role="alert">{writing.error}</p>{/if}
{/snippet}

<SettingRow bind:this={row} title={policyTitle(policy)} text={policyText(policy)}
            status={done && done.enabled === policy.enabled ? done.said : ""}
            children={edits || writing.error ? body : undefined}>
  {#snippet control(ids)}
    <StatusPill status={policy.enabled ? "on" : "off"} />
    {#if save && policy.enabled}
      <button type="button" id="{uid}-switch" aria-labelledby="{uid}-switch {ids.title}"
              aria-describedby="{ids.text} {uid}-hint" aria-disabled={writing.busy}
              onclick={() => write(policyBody(policy, "off"), "Turned off.")}>Turn off</button>
      <div id="{uid}-hint" class="hint">{turnOffText(policy.kind)}</div>
    {:else if save && counts}
      <button type="button" id="{uid}-switch" aria-labelledby="{uid}-switch {ids.title}" aria-describedby={ids.text}
              aria-haspopup="dialog" aria-disabled={writing.busy} onclick={turnOn}>Turn on</button>
    {/if}
  {/snippet}
</SettingRow>
<ConfirmDialog bind:this={confirm} />
