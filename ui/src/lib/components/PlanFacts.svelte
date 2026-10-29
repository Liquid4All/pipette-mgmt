<script lang="ts">
  // One plan's record (`plan_body.html`): who filed it and when, its warnings and
  // its document. Its status and progress are the Overview's.
  import { planDocument } from "../api";
  import { when } from "../format";
  import { documentNote, withheldNotice } from "../plans";
  import { userLink } from "../routes";
  import { session } from "../session.svelte";
  import type { DocumentState, NamedPlanView } from "../api-types";
  import Clamped from "./Clamped.svelte";
  import { Loader } from "./work.svelte";

  let { plan, document, results }: {
    plan: NamedPlanView;
    /** The document's state, which the jobs read says; null until it has. */
    document: DocumentState | null;
    /** The server reads results, whose detail the withheld tier hides too. */
    results: boolean;
  } = $props();

  const names = $derived(plan.names ?? {});
  const note = $derived(document ? documentNote(document, plan.fleet_detail) : null);
  // The document names client ids and requirement sets, which the withheld tier hides (`docs/authentication.md`).
  const readable = $derived(document === "present" && plan.fleet_detail === "full");

  // Read when first opened, the lesser half of the page; a failed read is tried again on the next open.
  const source = new Loader<string>();
  const open = (event: Event) => {
    if ((event.currentTarget as HTMLDetailsElement).open && source.data === null) {
      source.run(() => planDocument(session.signer, plan.plan_id));
    }
  };
</script>

<dl>
  <dt>plan_id</dt><dd class="id">{plan.plan_id}</dd>
  <dt>created_at</dt><dd>{when(plan.created_at)}</dd>
  <dt>submitted_by</dt><dd>
    {#if plan.submitted_by}
      <a href={userLink(plan.submitted_by)}>{names[plan.submitted_by] ?? plan.submitted_by}</a>
      <div class="muted id">{plan.submitted_by}</div>
    {:else}<span class="muted">operator</span>{/if}
  </dd>
  <dt>terminal_at</dt><dd>{when(plan.terminal_at)}</dd>
</dl>

<!-- One anchor for the Overview header's link to the warnings. -->
{#if plan.warnings.length || plan.plan_warnings.length}
<div id="warnings">
{#if plan.warnings.length}
  <h3>Warnings when filed</h3>
  <p class="muted">As recorded at filing.</p>
  <ul>
    {#each plan.warnings as warning, i (i)}<li><Clamped text={`${warning.message} (${warning.job_ids.length} jobs)`} /></li>{/each}
  </ul>
{/if}

{#if plan.plan_warnings.length}
  <h3>Planner warnings</h3>
  <p class="muted">Raised when the server expanded the plan document.</p>
  <ul>
    {#each plan.plan_warnings as warning, i (i)}<li><Clamped text={warning} /></li>{/each}
  </ul>
{/if}
</div>
{/if}

{#if readable}
  <details ontoggle={open}>
    <summary>plan document</summary>
    {#if source.error}<p class="err" role="alert">{source.error}</p>
    {:else if source.data === null}<p class="muted">Loading…</p>
    {:else}<pre>{source.data}</pre>{/if}
  </details>
{:else if note}
  <p class:err={note.fault} class:muted={!note.fault}>{note.text}</p>
{/if}

{#if plan.fleet_detail === "withheld"}<p class="muted">{withheldNotice(results)}</p>{/if}
