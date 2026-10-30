<script lang="ts" module>
  import { absentOrg, type OrgRef } from "../orgs";
  import { session } from "../session.svelte";
  import type { OrgView } from "../api-types";

  /**
   * The organization a page under `reference` sits under, or an error saying
   * there is none. `GET /api/v1/orgs/{org}`'s `403` reads as not found too, unless the
   * session holds it, as a `fresh` read of the reader's own may be refused.
   */
  export async function scopedOrg(reference: string, fresh = false): Promise<OrgRef | OrgView> {
    const org = await session.resolveOrg(reference, fresh).catch((e) => {
      if (absentOrg(e, !!session.knownOrg(reference))) return null;
      throw e;
    });
    if (!org) throw new Error(`No organization ${reference}.`);
    return org;
  }
</script>

<script lang="ts">
  // A page under `/app/orgs/{org}/…`: its heading, and the organization the
  // address names (the classic `handle`), resolved before the page draws. `fresh`
  // reads the whole `OrgView` anew, for a page that shows its counts.
  import { untrack, type Snippet } from "svelte";
  import { orgTrail } from "../crumbs";
  import Loaded from "./Loaded.svelte";
  import PageHead from "./PageHead.svelte";
  import { Loader } from "./work.svelte";

  type Action = { href: string; label: string };

  let { reference, title, fresh = false, inline = false, action = null, children: page }: {
    reference: string;
    title: string;
    fresh?: boolean;
    /** The title is the trail's last step, as `PageHead` draws it. */
    inline?: boolean;
    /** The heading's one action, such as a create form, drawn once the organization resolves, from it. */
    action?: Action | ((org: OrgRef | OrgView) => Action) | null;
    children: Snippet<[OrgRef | OrgView]>;
  } = $props();

  const found = new Loader<OrgRef | OrgView>();
  // Read once: the page is drawn anew for another address.
  $effect(() => {
    const asked = reference;
    untrack(() => found.run(() => scopedOrg(asked, fresh)));
  });
</script>

<PageHead trail={orgTrail(found.data ?? reference)} {title} {inline}
          action={found.data && action ? (typeof action === "function" ? action(found.data) : action) : null} />
<Loaded of={found}>
  {#snippet children(org)}{@render page(org)}{/snippet}
</Loaded>
