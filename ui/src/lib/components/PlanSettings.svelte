<script lang="ts">
  // A plan's Settings tab, on its own pages and its campaign's. It holds the
  // writes over the whole plan:
  // - its note;
  // - re-runs and recalculations, where the jobs read names a campaign for them;
  // - its cancel;
  // - on a campaign's page, the campaign's rename and transfer.
  // Every write but the rename is confirmed in one dialog, a danger-zone write
  // only once its phrase is typed.
  import type { Snippet } from "svelte";
  import { cancelPlan } from "../api";
  import { PHRASES } from "../danger";
  import { cancelOffered } from "../plans";
  import { REVISION, failedCount, type Filing } from "../rerun";
  import { Filer } from "../rerun.svelte";
  import { session } from "../session.svelte";
  import type { TabBase } from "../tabs";
  import type { PlanJobsWithResults } from "../api";
  import type { NamedPlanView } from "../api-types";
  import ConfirmDialog, { type Asked } from "./ConfirmDialog.svelte";
  import PlanNote from "./PlanNote.svelte";
  import SettingRow from "./SettingRow.svelte";
  import DangerZone from "./DangerZone.svelte";
  import SettingsSection from "./SettingsSection.svelte";
  import { Busy } from "./work.svelte";

  let { base, plan, jobs, again, onchange, general, transfer }: {
    base: TabBase;
    plan: NamedPlanView;
    /** Every job of the plan, which the screen reads once for all its tabs, and why it has not. */
    jobs: { data: PlanJobsWithResults | null; error: string };
    /** Read the jobs again, after a failed read. */
    again: Snippet;
    /** Read the plan again, after a write. */
    onchange: () => Promise<void>;
    /** On a campaign's page whose reader administers it, its rename, and its transfer in the danger zone. */
    general?: Snippet;
    transfer?: Snippet;
  } = $props();

  const data = $derived(jobs.data);
  // Drawn once the read settles, so no row moves into a box already on screen.
  const settled = $derived(!!data || !!jobs.error);
  // The campaign this plan heads, where the reader may re-run its jobs (`campaigns::may_rerun`).
  const rerunIn = $derived(data?.rerun_campaign_id ?? null);
  const cancel = $derived(!!data && cancelOffered(plan.status, data.may_cancel));
  // Counted, and off at none, only where the read settles every answered job.
  const failed = $derived(data ? failedCount(data.jobs, data.results_read, plan.status) : null);
  const terminal = $derived(plan.status === "complete" || plan.status === "cancelling" || plan.status === "cancelled");

  const filer = new Filer();
  const cancelling = new Busy();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  let cancelRow = $state<ReturnType<typeof SettingRow>>();

  /** A filing's confirmation, which opens the revision's Jobs once filed. */
  function filing(which: string, filed: Filing, question: Omit<Asked, "run" | "pending">): Asked {
    return {
      ...question, pending: "Filing…",
      run: () => (rerunIn ? filer.revise(which, rerunIn, plan.plan_id, filed, onchange) : Promise.resolve(null)),
    };
  }
  const rerunFailed = () => filing("failed", { verb: "rerun", body: { answered: "failed" } }, {
    title: "Re-run failed jobs?", action: "Re-run failed jobs",
    text: `Every job whose recorded result is a failure runs again on a device. ${REVISION}`,
  });
  const recalculateAll = () => filing("evals", { verb: "recalculate", body: { all_evals: true } }, {
    title: "Recalculate all finished evals?", action: "Recalculate all finished evals", phrase: PHRASES.evals,
    text: `The recorded completions of every finished eval are scored again under new jobs, which no device runs. ${
      REVISION}`,
  });
  const rerunAll = () => filing("all", { verb: "rerun", body: { answered: "all" } }, {
    title: "Re-run all finished and failed jobs?", action: "Re-run all finished and failed jobs", phrase: PHRASES.all,
    text: `Every finished and failed job runs again on a device. ${REVISION}`,
  });
  const cancelIt: Asked = {
    title: "Cancel this plan?", action: "Cancel this plan", pending: "Cancelling…", phrase: PHRASES.cancel,
    text: "The next queue-maintenance run stops handing out its jobs and tears it down, and a cancelled plan cannot "
      + "be resumed. Results already recorded stay.",
    // The answer holds no plan, so the plan is read again.
    run: async () => {
      await cancelling.attempt(async () => {
        await cancelPlan(session.signer, plan.plan_id);
        await onchange();
      });
      return cancelling.error || null;
    },
    // Its button is gone once requested, so the row's word on it takes the focus.
    done: () => cancelRow?.focusStatus(),
  };

  /** Open `question`, unless a write is under way: behind `main.ts`, which drops a locked control's click. */
  function ask(question: () => Asked, locked = false) {
    if (filer.busy || cancelling.busy || locked) return;
    void confirm?.ask(question());
  }
  /** A filing control's label, or "Filing…" while it files. */
  const label = (which: string, text: string) => (filer.which === which ? "Filing…" : text);
</script>

<div class="settings">
  {#if general}<SettingsSection title="General">{@render general()}</SettingsSection>{/if}

  {#if jobs.error && !data}
    <p class="err" role="alert">{jobs.error}</p>
    <p>{@render again()}</p>
  {:else if !data}
    <p class="muted">Reading the jobs…</p>
  {/if}

  {#if data?.may_edit_note || rerunIn}
    <SettingsSection title="Plan">
      {#if data?.may_edit_note}<PlanNote {plan} {onchange} />{/if}
      {#if rerunIn}
        <SettingRow title="Re-run failed jobs"
                    text="Runs every job whose recorded result is a failure again on a device, in a revision that replaces this plan.">
          {#snippet control()}
            <button type="button" aria-haspopup="dialog" aria-disabled={filer.busy || failed === 0}
                    title={failed === 0 ? "No job of this plan has failed" : undefined}
                    onclick={() => ask(rerunFailed, failed === 0)}>{
              label("failed", failed === null ? "Re-run failed jobs" : `Re-run failed jobs (${failed})`)}</button>
          {/snippet}
        </SettingRow>
      {/if}
    </SettingsSection>
  {/if}

  {#if settled && (rerunIn || cancel || transfer)}
    <DangerZone>
      {#if rerunIn}
        <SettingRow title="Recalculate all finished evals"
                    text="Scores the recorded completions of every finished eval again, in a revision that replaces this plan. No device runs them.">
          {#snippet control()}
            <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={filer.busy}
                    onclick={() => ask(recalculateAll)}>{label("evals", "Recalculate all finished evals")}</button>
          {/snippet}
        </SettingRow>
        <SettingRow title="Re-run all finished and failed jobs"
                    text="Queues every answered job on devices again, in a revision that replaces this plan. Jobs not yet answered carry over as they are.">
          {#snippet control()}
            <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={filer.busy}
                    onclick={() => ask(rerunAll)}>{label("all", "Re-run all finished and failed jobs")}</button>
          {/snippet}
        </SettingRow>
      {/if}
      {#if cancel}
        <SettingRow bind:this={cancelRow} title="Cancel this plan"
                    text="Stops handing out its jobs at the next queue-maintenance run and tears the plan down. A cancelled plan cannot be resumed; results already recorded stay."
                    status={plan.cancel_requested ? "Cancel requested: the next queue-maintenance run tears the plan down." : ""}>
          {#snippet control()}
            {#if !plan.cancel_requested}
              <button type="button" class="danger" aria-haspopup="dialog" aria-disabled={cancelling.busy}
                      onclick={() => ask(() => cancelIt)}>{cancelling.busy ? "Cancelling…" : "Cancel this plan"}</button>
            {/if}
          {/snippet}
        </SettingRow>
      {/if}
      {@render transfer?.()}
    </DangerZone>
  {/if}

  {#if data && !general && !transfer && !rerunIn && !cancel && !data.may_edit_note}
    <p class="muted">
      {#if base.campaign_id}This campaign belongs to someone else: you may read it and not change it.
      {:else if terminal}This plan is {plan.status}: nothing is left to change.
      {:else}Cancelling this plan needs <code>{data.cancel_permission}</code>.{/if}
    </p>
  {/if}
</div>
<ConfirmDialog bind:this={confirm} status={filer.busy ? filer.status : ""} />
