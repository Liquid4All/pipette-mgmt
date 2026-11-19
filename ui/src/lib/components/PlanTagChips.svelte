<script lang="ts">
  // A plan's tags, newest first, each a chip titled with its message.
  import type { PlanTag } from "../api-types";

  let { tags }: { tags: PlanTag[] } = $props();
</script>

{#each tags as tag (tag.name)}<span class="tag" title={tag.message}>{tag.name}</span>{" "}{/each}
