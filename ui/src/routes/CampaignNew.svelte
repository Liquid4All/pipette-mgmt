<script lang="ts">
  // A campaign and its first plan from a plan document checked as typed
  // (`POST /api/v1/plans/validate`), created through `POST /api/v1/campaigns`.
  import { createCampaign } from "../lib/api";
  import {
    CAMPAIGN_PAGES, chosenOwner, createQuery, importKey, importLine, nameProblem, newCampaignOwners, newCampaignRefusal,
    noteProblem,
  } from "../lib/campaigns";
  import ConfirmDialog from "../lib/components/ConfirmDialog.svelte";
  import PlanEditor from "../lib/components/PlanEditor.svelte";
  import { Busy } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { ownerBranch, underTrail } from "../lib/owned";
  import { PLAN_EXAMPLES } from "../lib/plan-examples";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { campaignLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route: _ }: { route: RouteOf<"campaign-new"> } = $props();

  const uid = $props.id();
  const reader = $derived(session.account!);
  const refusal = $derived(newCampaignRefusal(reader));
  const owners = $derived(newCampaignOwners(reader));
  // The listing the page was opened from, which its crumbs lead back to.
  const opened = chosenOwner(newCampaignOwners(session.account!), here.query.get("owner"));
  const under = $derived(ownerBranch(reader, opened ? { type: "org", org_id: opened }
    : { type: "user", user_id: reader.user_id }, session.known, CAMPAIGN_PAGES));
  $effect(() => { here.under = refusal ? null : under.href; });

  let name = $state("");
  let owner = $state(opened);
  let note = $state("");
  let text = $state("");
  /** Whether the name's problem shows: once something typed there is left, or a create is refused for it. */
  let touched = $state(false);
  let submitted = $state(false);
  const nameIssue = $derived(touched || submitted ? nameProblem(name) : "");
  const noteIssue = $derived(noteProblem(note));
  let nameInput = $state<HTMLInputElement>();
  let noteInput = $state<HTMLTextAreaElement>();
  let editor = $state<ReturnType<typeof PlanEditor>>();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  const work = new Busy();
  /** The text for each file that the import downloads. Each progress event replaces the text for its file. */
  let lines = $state<Record<string, string>>({});
  // Leaving the page stops the request. An import that still runs then stops,
  // and the server creates no campaign.
  const gone = new AbortController();
  $effect(() => () => gone.abort());

  async function create(event: SubmitEvent) {
    event.preventDefault();
    if (work.busy) return;
    submitted = true;
    if (nameProblem(name)) return nameInput?.focus();
    if (noteIssue) return noteInput?.focus();
    if (!text.trim()) return editor?.focus();
    // Read before the check: only the editor is locked while it runs.
    const toml = text;
    const query = createQuery({ name, owner, note });
    const from = here.path;
    await work.attempt(async () => {
      if (!(await editor?.check())) return;
      lines = {};
      const created = await createCampaign(session.signer, query, toml,
                                           (line) => { lines[importKey(line)] = importLine(line); }, gone.signal)
        .catch((e: unknown) => {
          // A failed filing stops its downloads, so no line may show one as running.
          lines = {};
          throw e;
        });
      // Expanding and ingesting the plan takes seconds; the reader may have moved on.
      navigateFrom(from, campaignLink(created.campaign_id));
    });
  }
</script>

<PageHead trail={refusal ? [] : underTrail(under, "Campaigns")} title="New campaign" />
{#if refusal}
  <p class="err" role="alert">{refusal}</p>
{:else}
  <form novalidate onsubmit={create}>
    <label><span>name</span>
      <input type="text" autocomplete="off" placeholder="Nightly LFM2 regression" bind:value={name}
             bind:this={nameInput} onblur={() => (touched ||= name !== "")}
             aria-invalid={!!nameIssue || undefined} aria-describedby="{uid}-name">
    </label>
    <div id="{uid}-name" class="hint err" role="status" hidden={!nameIssue}>{nameIssue}</div>
    {#if owners.length > 1}
      <label><span>owner</span>
        <select bind:value={owner}>
          {#each owners as option (option.value)}<option value={option.value}>{option.label}</option>{/each}
        </select>
      </label>
    {/if}
    <PlanEditor bind:text confirm={(asked) => confirm?.ask(asked)} verb="create"
                examples={PLAN_EXAMPLES} required={submitted} locked={work.busy} bind:this={editor} />
    <label><span>note (optional)</span>
      <textarea rows="2" placeholder="Why this campaign runs" bind:value={note} bind:this={noteInput}
                aria-invalid={!!noteIssue || undefined} aria-describedby="{uid}-note"></textarea>
    </label>
    <div id="{uid}-note" class="hint err" role="status" hidden={!noteIssue}>{noteIssue}</div>
    {#if work.error}<p class="err code" role="alert">{work.error}</p>{/if}
    <div class="bar">
      <button type="submit" class="primary" aria-disabled={work.busy}>{work.busy ? "Creating…" : "Create campaign"}</button>
    </div>
  </form>
  {#if Object.keys(lines).length}
    <ol class="muted" aria-live="polite">
      {#each Object.entries(lines) as [key, line] (key)}<li>{line}</li>{/each}
    </ol>
  {/if}
  <ConfirmDialog bind:this={confirm} />
{/if}
