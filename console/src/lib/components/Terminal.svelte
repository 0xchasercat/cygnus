<script>
  // A light terminal, done properly — porcelain well, mono, quiet timestamps.
  let { lines = [], building = false, maxHeight = '460px' } = $props();
</script>

<div class="term" style="max-height:{maxHeight}">
  {#each lines as l}
    <div class="line {l.kind}">
      <span class="t num">{l.t}s</span>
      <span class="txt">{l.text}</span>
    </div>
  {/each}
  {#if building}
    <div class="line">
      <span class="t num">&nbsp;</span>
      <span class="txt caret"></span>
    </div>
  {/if}
</div>

<style>
  .term {
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: var(--r-m);
    padding: 16px 18px;
    overflow-y: auto;
    font-family: var(--mono);
    font-size: 12px;
    line-height: 1.9;
  }
  .line {
    display: flex;
    gap: 16px;
    white-space: pre-wrap;
  }
  .t {
    color: var(--ink-4);
    flex: none;
    width: 44px;
    text-align: right;
    font-size: 11px;
    user-select: none;
  }
  .txt { color: var(--ink-2); }
  .head .txt {
    color: var(--ink);
    font-weight: 600;
  }
  .dim .txt { color: var(--ink-3); }
  .ok .txt { color: #087a45; }
  .err .txt { color: var(--red); }

  .caret::after {
    content: '';
    display: inline-block;
    width: 7px;
    height: 13px;
    background: var(--ink-2);
    vertical-align: -2px;
    animation: caret-blink 1.1s steps(1) infinite;
  }
</style>
