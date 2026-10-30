<script lang="ts">
  // The plans a revision inherits finished runs from (`inherit_from`): a campaign
  // the reader lists adds the plan it runs, and any plan the reader reads joins by its id.
  import { ApiError, campaigns, plan } from "../api";
  import { inheritOffers, namedOffer, planSourceOf, sourceText, type InheritSource } from "../campaigns";
  import { planLink } from "../routes";
  import { session } from "../session.svelte";
  import { ROUTE_PAGE_MAX, everyPage, withCursor } from "../table";
  import { describe } from "./guard";

  let { sources = $bindable([]), except, disabled = false, onvariants }: {
    sources?: InheritSource[];
    /** The campaign under revision, whose own plans `carry_earlier` offers. */
    except: string;
    disabled?: boolean;
    /** Appends the variants of the plan added, which offers "Add with its variants". */
    onvariants?: (source: InheritSource) => Promise<void>;
  } = $props();

  const uid = $props.id();
  let typed = $state("");
  let error = $state("");
  let adding = $state(false);
  let offers = $state.raw<InheritSource[]>([]);
  // Read once, on the field's first focus, since most revisions inherit nothing.
  let listing: Promise<InheritSource[]> | null = null;
  const listed = () => (listing ??= everyPage((cursor) => campaigns(session.signer,
    withCursor(`order=newest&plan=true&limit=${ROUTE_PAGE_MAX}`, cursor)))
    .then((page) => (offers = inheritOffers(page.items, except)))
    .catch(() => {
      // Listed again on the next focus.
      listing = null;
      return (offers = []);
    }));

  async function sourceNamed(asked: string): Promise<InheritSource> {
    const offered = namedOffer(await listed(), asked);
    if (offered) return offered;
    try {
      return planSourceOf(await plan(session.signer, asked), offers);
    } catch (e) {
      if (e instanceof ApiError && (e.status === 404 || e.status === 400)) {
        throw new Error(`No campaign or plan you can read is called \`${asked}\``);
      }
      throw e;
    }
  }

  async function add(withVariants = false) {
    const asked = typed.trim();
    if (!asked || adding || disabled) return;
    error = "";
    adding = true;
    try {
      const source = await sourceNamed(asked);
      const listed = sources.some((known) => known.plan_id === source.plan_id);
      if (listed && !withVariants) {
        error = `${source.plan_id} is already listed`;
        return;
      }
      if (withVariants) await onvariants?.(source);
      if (!listed) sources = [...sources, source];
      typed = "";
    } catch (e) {
      error = describe(e);
    } finally {
      adding = false;
    }
  }

  function keydown(event: KeyboardEvent) {
    if (event.key !== "Enter") return;
    // Adds the plan rather than filing the revision.
    event.preventDefault();
    void add();
  }
</script>

<fieldset>
  <legend>inherit finished runs from (optional)</legend>
  {#if sources.length}
    <ul class="sources">
      {#each sources as source (source.plan_id)}
        <li>
          <a href={planLink(source.plan_id)}>{sourceText(source)}</a>
          <span class="muted id">{source.plan_id}</span>
          <button type="button" aria-label="Remove {source.plan_id}" aria-disabled={disabled}
                  onclick={() => { if (!disabled) sources = sources.filter((kept) => kept !== source); }}>Remove</button>
        </li>
      {/each}
    </ul>
  {/if}
  <div class="bar">
    <label><span>campaign or plan</span>
      <input type="text" autocomplete="off" spellcheck="false" placeholder="a campaign's name, or plan-…"
             list="{uid}-offers" bind:value={typed} {disabled} aria-invalid={!!error || undefined}
             aria-describedby="{uid}-inherit-hint" onfocus={listed} onkeydown={keydown}>
    </label>
    <button type="button" aria-disabled={disabled || adding || !typed.trim()} onclick={() => add()}>{
      adding ? "Adding…" : "Add"}</button>
    {#if onvariants}
      <button type="button" aria-disabled={disabled || adding || !typed.trim()} onclick={() => add(true)}>
        Add with its variants</button>
    {/if}
  </div>
  <datalist id="{uid}-offers">
    {#each offers as offer (offer.plan_id)}<option value={offer.plan_id}>{sourceText(offer)}</option>{/each}
  </datalist>
  <div id="{uid}-inherit-hint" class="hint" class:err={!!error} role="status">{error ||
    "A cell the current plan has not answered takes a finished run of these plans that still meets its requirements. "
    + "A campaign adds the plan it runs now."
    + (onvariants ? " \"Add with its variants\" also appends the plan's variants to the document, so it runs their cells." : "")}</div>
</fieldset>

<style>
  .sources { list-style: none; margin: 4px 0; padding: 0; }
  .sources li { display: flex; gap: 8px; align-items: baseline; padding: 2px 0; }
  .sources li button { margin-left: auto; }
</style>
