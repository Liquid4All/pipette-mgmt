<script lang="ts" generics="N extends string">
  // A permission form's boxes, a name each with its note; a box not offered is drawn and left alone.
  let { boxes, toggle }: {
    boxes: { name: N; checked: boolean; offered: boolean; note: string | null }[];
    toggle: (name: N, on: boolean) => void;
  } = $props();

  const uid = $props.id();
</script>

<table class="fit">
  <tbody>
    {#each boxes as box (box.name)}
      <tr>
        <td><input type="checkbox" id="{uid}-{box.name}" disabled={!box.offered} checked={box.checked}
                   onchange={(event) => toggle(box.name, event.currentTarget.checked)}></td>
        <td><label for="{uid}-{box.name}"><code>{box.name}</code></label></td>
        <td class="muted">{box.note ?? ""}</td>
      </tr>
    {/each}
  </tbody>
</table>
