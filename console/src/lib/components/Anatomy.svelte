<script>
  // Cold-start decomposition — the spec's §12 budget as an instrument.
  let { phases = [] } = $props();

  const total = $derived(phases.reduce((s, p) => s + p.ms, 0));
  const QUIET = ['#d5d9e2', '#c3c9d6', '#e2e5ec', '#cdd2dd', '#dde1e9'];
</script>

<div class="anatomy">
  <div class="bar">
    {#each phases as p, i}
      <i
        style="flex:{p.ms};background:{p.hot ? 'var(--cobalt)' : QUIET[i % QUIET.length]}"
        title="{p.name} · {p.ms} ms"
      ></i>
    {/each}
  </div>
  <div class="legend">
    {#each phases as p, i}
      <div class="row">
        <i style="background:{p.hot ? 'var(--cobalt)' : QUIET[i % QUIET.length]}"></i>
        <span class="name">{p.name}</span>
        <span class="ms num">{p.ms.toFixed(1)} ms</span>
      </div>
    {/each}
    <div class="row total">
      <i style="background:transparent"></i>
      <span class="name">request → first user-code byte</span>
      <span class="ms num">{total.toFixed(1)} ms</span>
    </div>
  </div>
</div>

<style>
  .bar {
    display: flex;
    gap: 2px;
    height: 10px;
    border-radius: 5px;
    overflow: hidden;
  }
  .bar i {
    display: block;
    min-width: 3px;
  }
  .bar i:first-child { border-radius: 5px 0 0 5px; }
  .bar i:last-child { border-radius: 0 5px 5px 0; }

  .legend {
    margin-top: 14px;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 9px;
  }
  .row i {
    width: 8px;
    height: 8px;
    border-radius: 2.5px;
    flex: none;
  }
  .name {
    font-size: 12px;
    color: var(--ink-2);
    flex: 1;
  }
  .ms {
    font-size: 11.5px;
    color: var(--ink);
  }
  .total {
    border-top: 1px solid var(--line-2);
    padding-top: 8px;
    margin-top: 2px;
  }
  .total .name {
    color: var(--ink-3);
    font-size: 11.5px;
  }
  .total .ms {
    font-weight: 600;
    color: var(--cobalt-deep);
  }
</style>
