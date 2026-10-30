<script lang="ts">
  // A campaign's plan document edited whole: the document of the plan it runs
  // from `GET /api/v1/campaigns/{id}/document`, checked as typed, dry-run and confirmed,
  // then filed through `PUT /api/v1/campaigns/{id}/document`.
  import { ApiError, campaign, campaignDocument, planVariants, previewDocument, reviseDocument } from "../lib/api";
  import {
    CAMPAIGN_PAGES, importKey, importLine, importText, noteProblem, repeatedText, REVISION_EFFECT, revisionQuery, revisionSummary,
    withVariantsOf, type InheritSource,
  } from "../lib/campaigns";
  import { changeText } from "../lib/plans";
  import ConfirmDialog from "../lib/components/ConfirmDialog.svelte";
  import InheritFrom from "../lib/components/InheritFrom.svelte";
  import { describe } from "../lib/components/guard";
  import JobsItStops from "../lib/components/JobsItStops.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PlanEditor from "../lib/components/PlanEditor.svelte";
  import { Busy, Loader } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { sameDocument } from "../lib/document";
  import { ownerBranch, underTrail } from "../lib/owned";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { campaignLink, campaignPlanLink, pathOf, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { CampaignDocument, NamedCampaignView, RevisionDryRun } from "../lib/api-types";

  let { route }: { route: RouteOf<"campaign-document"> } = $props();

  const uid = $props.id();
  const id = $derived(route.campaign_id);
  const page = new Loader<{ campaign: NamedCampaignView; read: CampaignDocument }>();
  // The document read's refusal is the page's, whichever read answers first.
  const load = async () => {
    const [record, read] = await Promise.allSettled([
      campaign(session.signer, id), campaignDocument(session.signer, id),
    ]);
    if (read.status === "rejected") throw read.reason;
    if (record.status === "rejected") throw record.reason;
    return { campaign: record.value, read: read.value };
  };

  let text = $state("");
  // An edit made against a plan the campaign has since moved off, kept to put back.
  let stale = $state<string | null>(null);
  let note = $state("");
  /** Whether cells the current plan has not answered may take earlier plans' finished runs. */
  let carryEarlier = $state(false);
  /** Plans of any campaign whose finished runs those cells may take. */
  let sources = $state<InheritSource[]>([]);
  const inherited = $derived(sources.map((source) => source.plan_id));
  /** Whether the revision takes runs beside the current plan's, which files even its own document. */
  const offers = $derived(carryEarlier || inherited.length > 0);
  /** The dry run the confirmation asks about. */
  let reviewed = $state.raw<RevisionDryRun | null>(null);
  /** Why filing `said.text` would do nothing, shown while the editor still holds it. */
  let said = $state<{ text: string; message: string } | null>(null);
  const noteIssue = $derived(noteProblem(note));
  let noteInput = $state<HTMLTextAreaElement>();
  let editor = $state<ReturnType<typeof PlanEditor>>();
  let confirm = $state<ReturnType<typeof ConfirmDialog>>();
  const work = new Busy();
  /** The text for each file that the import downloads. Each progress event replaces the text for its file. */
  let lines = $state<Record<string, string>>({});
  // Leaving the page stops the request. An import that still runs then stops,
  // and the server files no revision.
  const gone = new AbortController();
  $effect(() => () => gone.abort());
  // The text is set with the read, so the editor never draws empty first.
  const show = (read = load, keep = false) => page.run(async () => {
    const data = await read();
    text = data.read.document;
    return data;
  }, keep);
  show();

  const record = $derived(page.data?.campaign ?? null);
  const under = $derived(record && session.account ? ownerBranch(session.account, record.owner, session.known, CAMPAIGN_PAGES) : null);
  $effect(() => { here.under = under?.href ?? null; });
  const unchanged = $derived(!!page.data && sameDocument(text, page.data.read.document));

  /**
   * Whether `e` says the campaign moved off `base`: then the editor takes what
   * it runs now, and the edit is kept beside it rather than filed over a revision unread.
   */
  async function moved(e: unknown, base: string, edit: string): Promise<boolean> {
    if (!(e instanceof ApiError && e.status === 409)) return false;
    // Other conflicts answer 409 too, such as an owner change: only a new base is a move.
    const fresh = await load().catch(() => null);
    if (!fresh || fresh.read.base.plan_id === base) return false;
    await show(async () => fresh, true);
    stale = sameDocument(text, edit) ? null : edit;
    return true;
  }

  async function file(event: SubmitEvent) {
    event.preventDefault();
    const base = page.data?.read.base.plan_id;
    if (!base || work.busy || (unchanged && !offers)) return;
    if (noteIssue) return noteInput?.focus();
    if (!text.trim()) return editor?.focus();
    const edit = text;
    const query = revisionQuery(base, note, carryEarlier, inherited);
    const from = pathOf(route);
    said = null;
    await work.attempt(async () => {
      if (!(await editor?.check())) return;
      let preview: RevisionDryRun;
      try {
        preview = await previewDocument(session.signer, id, query, edit);
      } catch (e) {
        if (await moved(e, base, edit)) return;
        throw e;
      }
      if (preview.unchanged) {
        said = { text: edit, message: revisionSummary(preview) };
        return;
      }
      reviewed = preview;
      confirm?.ask({
        title: "File this revision?",
        text: `${revisionSummary(preview)} ${REVISION_EFFECT}`,
        fields,
        action: "File revision",
        pending: "Filing…",
        run: async () => {
          lines = {};
          try {
            await reviseDocument(session.signer, id, query, edit,
                                 (line) => { lines[importKey(line)] = importLine(line); }, gone.signal);
            return null;
          } catch (e) {
            // A failed filing stops its downloads, so no line may show one as running.
            lines = {};
            if (await moved(e, base, edit)) {
              return "The campaign moved to another plan while you reviewed. Close this and check its document.";
            }
            return describe(e);
          }
        },
        done: () => navigateFrom(from, campaignLink(id)),
      });
    });
  }

  /** Appends the variants of `source`'s plan, which then answers their cells. */
  const appendVariants = async (source: InheritSource) => {
    text = withVariantsOf(text, source.plan_id, await planVariants(session.signer, source.plan_id));
  };

  // Through the editor, which asks before replacing what was typed since.
  const useEdit = (kept: string) => editor?.replace(kept, "your edit", null, () => (stale = null));
</script>

{#snippet fields()}
  {#if reviewed?.imports.length}
    <h3>Imports</h3>
    <ul>
      {#each reviewed.imports as entry, index (index)}<li>{importText(entry)}</li>{/each}
    </ul>
  {/if}
  {#if reviewed?.repeated_variants.length}<p class="muted">{repeatedText(reviewed.repeated_variants)}</p>{/if}
  {#if reviewed}<JobsItStops ids={reviewed.dropped_job_ids} />{/if}
  {#if Object.keys(lines).length}
    <ol class="muted" aria-live="polite">
      {#each Object.entries(lines) as [key, line] (key)}<li>{line}</li>{/each}
    </ol>
  {/if}
{/snippet}

<!-- The id until the name is read: drawn at once, the heading keeps the focus navigation gave it. -->
<PageHead trail={record && under ? [...underTrail(under, "Campaigns"), { label: record.name, href: campaignLink(id) }] : []}
          title={`Plan document of ${record?.name ?? id}`} />

<Loaded of={page}>
  {#snippet children({ read })}
    <dl class="summary">
      <div><dt>plan</dt><dd>
        <a href={campaignPlanLink(id, read.base.plan_id)} class="id">{read.base.plan_id}</a>
        <span class="tag current">current</span>
        {#if read.base.change}<div class="muted">{changeText(read.base.change)}</div>{/if}
        {#if read.base.note}<div class="muted note">{read.base.note}</div>{/if}
      </dd></div>
    </dl>

    {#if stale}
      <div class="notice">
        <p role="status">The campaign moved to {read.base.plan_id} while you edited, so the editor now holds
          that plan's document. Your edit is kept below.</p>
        <details open>
          <summary>Your edit</summary>
          <pre>{stale}</pre>
        </details>
        <button type="button" aria-disabled={work.busy} onclick={() => stale && useEdit(stale)}>Use my edit</button>
      </div>
    {/if}
    <form novalidate onsubmit={file}>
      <PlanEditor bind:text confirm={(asked) => confirm?.ask(asked)} verb="file"
                  original={read.document} locked={work.busy} bind:this={editor} />
      <label><span>note (optional)</span>
        <textarea rows="2" placeholder="Why this revision: added the 8-bit variants" bind:value={note}
                  bind:this={noteInput} aria-invalid={!!noteIssue || undefined}
                  aria-describedby="{uid}-note-hint {uid}-note"></textarea>
      </label>
      <div id="{uid}-note-hint" class="hint">Kept as you write it; the server records what the revision changes beside it.</div>
      <div id="{uid}-note" class="hint err" role="status" hidden={!noteIssue}>{noteIssue}</div>
      <label class="check"><input type="checkbox" bind:checked={carryEarlier} disabled={work.busy}>
        <span>Bring back finished runs from the campaign's earlier plans, for cells the current plan has not
          answered</span></label>
      <InheritFrom bind:sources except={id} disabled={work.busy} onvariants={appendVariants} />
      {#if work.error}<p class="err code" role="alert">{work.error}</p>{/if}
      <div class="bar">
        <button type="submit" class="primary" aria-disabled={work.busy || (unchanged && !offers)}>{
          work.busy ? "Checking…" : "File revision…"}</button>
        {#if said && sameDocument(text, said.text)}<span class="muted" role="status">{said.message}</span>{/if}
      </div>
    </form>
    <ConfirmDialog bind:this={confirm} />
  {/snippet}
</Loaded>

<style>
  .notice p { margin: 0; }
  .notice details pre { color: var(--text); }
  .note { white-space: pre-wrap; }
</style>
