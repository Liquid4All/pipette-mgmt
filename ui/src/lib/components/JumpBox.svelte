<script lang="ts">
  // The header's box: a pasted ID opens its record's page (`jump.ts`). `/` focuses it.
  import { onDestroy } from "svelte";
  import { jobPlans, plan } from "../api";
  import { resolve } from "../jump";
  import { here, navigateFrom } from "../route.svelte";
  import { session } from "../session.svelte";
  import { Busy } from "./work.svelte";

  let text = $state("");
  let mounted = true;
  onDestroy(() => { mounted = false; });
  const jumping = new Busy();

  const look = {
    jobPlans: async (job_id: string) => (await jobPlans(session.signer, job_id)).plans,
    plan: (plan_id: string) => plan(session.signer, plan_id),
  };

  function submit(event: SubmitEvent) {
    event.preventDefault();
    if (jumping.busy || !text.trim()) return;
    const submitted = text;
    const from = here.path;
    void jumping.attempt(async () => {
      const address = await resolve(submitted, look);
      if (!mounted) return;
      const stillHere = here.path === from;
      navigateFrom(from, address);
      if (stillHere && text === submitted) text = "";
    });
  }
</script>

<form class="jump" role="search" onsubmit={submit}>
  <label class="sr" for="jump">Open an ID</label>
  <input id="jump" type="text" placeholder="Paste an ID  /" autocomplete="off" spellcheck="false"
         bind:value={text} oninput={() => (jumping.error = "")} aria-busy={jumping.busy}
         aria-invalid={!!jumping.error} aria-describedby={jumping.error ? "jump-error" : undefined} />
  {#if jumping.error}<p id="jump-error" class="err" role="alert">{jumping.error}</p>{/if}
</form>

<style>
  .jump { position: relative; }
  .jump input { width: 22em; max-width: 40vw; box-sizing: border-box; }
  /* Under the box, over the page, so the header keeps its height. */
  .err { position: absolute; top: 100%; right: 0; width: max-content; max-width: 40em; margin: 4px 0 0; }
</style>
