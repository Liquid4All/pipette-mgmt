<script lang="ts">
  // An owner inside the caller's cell, an organization: its name linking to its
  // page, the id on hover. A name comes from the body's `names` (an organization's
  // display name); without one the id stands in. It links by its id.
  import { principalOf } from "../owned";
  import { pathOf } from "../routes";
  import type { ClientOwner } from "../api-types";

  let { owner, names = {} }: { owner: ClientOwner; names?: Record<string, string> } = $props();
  const id = $derived(principalOf(owner));
  const name = $derived(names[id]);
</script>

<a class:id={!name} href={pathOf({ page: "org", org: id })} title={name ? id : undefined}>{name ?? id}</a>
