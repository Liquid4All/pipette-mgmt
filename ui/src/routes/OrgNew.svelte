<script lang="ts">
  // The classic console's new-organization form (`static/templates/new_org.html`):
  // `POST /api/v1/orgs`, then the organization it created.
  import { createOrg } from "../lib/api";
  import { Busy } from "../lib/components/work.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import { ORGS } from "../lib/crumbs";
  import { createBody, createOrgRefusal } from "../lib/orgs";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { orgLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";

  let { route: _ }: { route: RouteOf<"org-new"> } = $props();

  const refusal = $derived(createOrgRefusal(session.account!));
  // The form sits under Server > Organizations, as `new_org.html` marks it; a refusal under nothing.
  $effect(() => { here.under = refusal ? null : "/app/orgs"; });
  let slug = $state("");
  let displayName = $state("");
  const work = new Busy();

  async function create(event: SubmitEvent) {
    event.preventDefault();
    await work.attempt(async () => {
      const from = here.path;
      const created = await createOrg(session.signer, createBody(slug, displayName));
      // The tree lists it among the others from now on.
      await session.refresh();
      navigateFrom(from, orgLink(created));
    });
  }
</script>

<PageHead trail={refusal ? [] : [ORGS]} title="New organization" />
{#if refusal}
  <p class="err" role="alert">{refusal}</p>
{:else}
  <p class="muted">
    Use up to 64 ASCII letters, digits, hyphens, or underscores. The handle is stored
    in lowercase and cannot change. It cannot start with <code>org_</code>.
    The descriptor catalog reserves <code>global</code> for <code>global/&lt;name&gt;</code>.
    You can change the display name later.
  </p>
  {#if work.error}<p class="err" role="alert">{work.error}</p>{/if}
  <form onsubmit={create}>
    <label><span>handle</span>
      <input type="text" bind:value={slug} required autocomplete="off" spellcheck="false" placeholder="acme">
    </label>
    <label><span>display name, the handle when left empty</span>
      <input type="text" bind:value={displayName} autocomplete="off" placeholder="Acme Corp.">
    </label>
    <button type="submit" class="primary" aria-disabled={work.busy}>Create this organization</button>
  </form>
{/if}
