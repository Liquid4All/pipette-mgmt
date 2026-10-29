<script lang="ts">
  // An organization's settings, read from `GET /api/v1/orgs/{org}/overview`: its
  // danger zone, drawn for its administrator: its rename, and its disable or
  // enable for a reader over every organization.
  import { orgOverview, updateOrg } from "../lib/api";
  import DangerRow from "../lib/components/DangerRow.svelte";
  import Loaded from "../lib/components/Loaded.svelte";
  import PageHead from "../lib/components/PageHead.svelte";
  import DangerZone from "../lib/components/DangerZone.svelte";
  import StatusPill from "../lib/components/StatusPill.svelte";
  import { Loader } from "../lib/components/work.svelte";
  import { ORGS } from "../lib/crumbs";
  import { ORG } from "../lib/danger";
  import { handle, when } from "../lib/format";
  import { absentOrg, renameBody } from "../lib/orgs";
  import { here, navigateFrom } from "../lib/route.svelte";
  import { orgLink, type RouteOf } from "../lib/routes";
  import { session } from "../lib/session.svelte";
  import type { OrgOverview } from "../lib/api-types";

  let { route }: { route: RouteOf<"org"> } = $props();

  const page = new Loader<OrgOverview>();
  const read = () => orgOverview(session.signer, route.org).catch((e) => {
    if (absentOrg(e, !!session.knownOrg(route.org))) throw new Error(`No organization ${route.org}.`);
    throw e;
  });
  page.run(read);

  // What the form drew, kept as typed after a refusal, as the classic `refused_org` redrew it.
  let displayName = $state("");
  $effect(() => {
    const drawn = page.data;
    if (drawn) displayName = drawn.display_name;
  });

  async function save(view: OrgOverview) {
    const from = here.path;
    const saved = await updateOrg(session.signer, view.org_id, renameBody(displayName));
    // By handle, whichever spelling the address used.
    if (handle(saved) === route.org) await page.run(read, true);
    else navigateFrom(from, orgLink(saved));
  }

  const setStatus = async (view: OrgOverview) => {
    await updateOrg(session.signer, view.org_id, { status: view.status === "active" ? "disabled" : "active" });
    await page.run(read, true);
  };
</script>

<!-- Drawn at once and kept, so the focus navigation gave it survives the read. -->
<PageHead trail={page.data ? [ORGS] : []} title={page.data?.display_name ?? "Organization"} />

<Loaded of={page}>
  {#snippet children(view)}
    <dl>
      <dt>org_id</dt><dd class="id">{view.org_id}</dd>
      <dt>slug</dt><dd>{view.slug}</dd>
      <dt>status</dt><dd><StatusPill status={view.status} /></dd>
      <dt>created_at</dt><dd>{when(view.created_at)}</dd>
    </dl>
    {#if view.may_administer}
      {#snippet name()}
        <label><span>display_name</span>
          <input type="text" bind:value={displayName} required autocomplete="off">
        </label>
      {/snippet}
      <div class="settings">
        <DangerZone>
          <DangerRow title="Rename this organization" ask={`Rename ${view.slug}?`} pending="Renaming…"
                     phrase={ORG.rename} fields={name} run={() => save(view)}
                     text="Changes the name listings and this page show. The handle is the organization's index key and does not change: one that needs another is a new organization." />
          {#if view.may_delete}
            {#if view.status === "active"}
              <DangerRow title="Disable this organization" ask={`Disable ${view.slug}?`} pending="Disabling…"
                         phrase={ORG.disabled} run={() => setStatus(view)}
                         text="Marks a wound-down organization: it takes no new member, and its members keep their access until their accounts are disabled." />
            {:else}
              <DangerRow title="Enable this organization" ask={`Enable ${view.slug}?`} pending="Enabling…"
                         phrase={ORG.active} run={() => setStatus(view)} text="Takes new members again." />
            {/if}
          {/if}
        </DangerZone>
      </div>
    {/if}
  {/snippet}
</Loaded>
