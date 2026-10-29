<script lang="ts">
  // The red Settings section every destructive write sits in.
  import type { Snippet } from "svelte";
  import SettingsSection from "./SettingsSection.svelte";

  let { children }: { children: Snippet } = $props();
</script>

<SettingsSection title="Danger zone" danger>{@render children()}</SettingsSection>
