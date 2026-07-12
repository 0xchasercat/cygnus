<script>
  // mini deploy-history bars: 1 ok · 2 ok-slow · 3 failed · 4 building
  // quiet by design — only failures and in-flight builds get a color
  let { data = [], h = 22 } = $props();

  const COLOR = { 1: '#dcdfe7', 2: '#c6ccd8', 3: 'var(--red)', 4: 'var(--violet)' };

  function height(v, i) {
    const base = v === 1 ? 0.45 + ((i * 37) % 40) / 100 : 0.95;
    return Math.round(base * (h - 4)) + 4;
  }
</script>

<span class="bars" style="height:{h}px" aria-hidden="true">
  {#each data as v, i}
    <i style="height:{height(v, i)}px;background:{COLOR[v] ?? COLOR[1]}"></i>
  {/each}
</span>

<style>
  .bars {
    display: inline-flex;
    align-items: flex-end;
    gap: 3px;
  }
  i {
    width: 3px;
    border-radius: 1.5px;
    display: block;
  }
</style>
