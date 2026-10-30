<script lang="ts">
  // An owner inside the caller's cell, an organization or an account alike: its
  // name linking to its page, the id on hover. A name comes from the body's
  // `names` (an organization's handle, an account's address); without one the
  // id stands in.
  import { orgLink, userLink } from "../routes";
  import type { ClientOwner } from "../api-types";

  let { owner, names = {} }: { owner: ClientOwner; names?: Record<string, string> } = $props();
</script>

{#if owner.type === "org"}
  {@const slug = names[owner.org_id]}
  <a class:id={!slug} href={orgLink({ slug: slug ?? owner.org_id, org_id: owner.org_id })}
     title={slug ? owner.org_id : undefined}>{slug ?? owner.org_id}</a>
{:else}
  {@const address = names[owner.user_id]}
  <a class:id={!address} href={userLink(owner.user_id)} title={address ? owner.user_id : undefined}>{address
    ?? owner.user_id}</a>
{/if}
