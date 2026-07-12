<script>
  import { deploys } from '../data.js';
  import { openDeploy } from '../stores.svelte.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';

  let filter = $state('all');

  const filtered = $derived(
    deploys.filter((d) => {
      if (filter === 'all') return true;
      if (filter === 'failed') return d.status === 'failed';
      if (filter === 'previews') return d.status === 'preview';
      return d.status === 'live' || d.status === 'previous' || d.status === 'building';
    })
  );

  const LED = {
    live: 'live',
    building: 'build',
    failed: 'fail',
    preview: 'preview',
    previous: 'cold',
  };
  const STATUS = {
    live: 'production',
    building: 'building',
    failed: 'failed',
    preview: 'preview',
    previous: 'retained',
  };
</script>

<div class="page screen-enter">
  <div class="head">
    <div>
      <h1>Deploys</h1>
      <p class="sub">Every artifact this node has built. Blue-green swaps, previous five retained for instant rollback.</p>
    </div>
    <div class="seg">
      <button class:on={filter === 'all'} onclick={() => (filter = 'all')}>All</button>
      <button class:on={filter === 'production'} onclick={() => (filter = 'production')}>Production</button>
      <button class:on={filter === 'previews'} onclick={() => (filter = 'previews')}>Previews</button>
      <button class:on={filter === 'failed'} onclick={() => (filter = 'failed')}>Failed</button>
    </div>
  </div>

  <section class="card">
    <div class="rows">
      {#each filtered as d (d.id)}
        <button class="row" onclick={() => openDeploy(d.app, d.id)}>
          <span class="led {LED[d.status]}" class:breathe={d.status === 'building'}></span>
          <span class="appcell">
            <Identicon name={d.app} size={22} />
            <span class="appname">{d.app}</span>
          </span>
          <span class="commit">{d.commit}</span>
          <span class="chip branch"><Icon name="branch" size={11} />{d.branch}</span>
          <span class="cellnum num who">{d.author}</span>
          <span class="cellnum num">{d.dur}</span>
          <span class="cellnum num">{d.size}</span>
          <span class="cellnum num when">{d.when}</span>
          <span class="status pill {LED[d.status] === 'cold' ? 'ghost' : LED[d.status]}">{STATUS[d.status]}</span>
          <span class="chev"><Icon name="chevR" size={13} /></span>
        </button>
      {/each}
    </div>
  </section>
</div>

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 26px 44px 0;
  }
  .head {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 20px;
    margin-bottom: 18px;
  }
  h1 {
    font-size: 23px;
    font-weight: 650;
    letter-spacing: -0.02em;
  }
  .sub {
    margin-top: 5px;
    font-size: 13px;
    color: var(--ink-3);
    max-width: 560px;
  }

  .rows { padding: 6px; }
  .row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 12px 14px;
    border-radius: 12px;
    text-align: left;
    transition: background 0.12s ease;
  }
  .row:hover { background: var(--surface-2); }
  .row + .row { border-top: 1px solid var(--line-2); }
  .row:hover + .row, .row:hover { border-top-color: transparent; }

  .appcell {
    display: flex;
    align-items: center;
    gap: 9px;
    width: 150px;
    flex: none;
  }
  .appname {
    font-size: 12.5px;
    font-weight: 650;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .commit {
    flex: 1;
    min-width: 0;
    font-size: 13px;
    color: var(--ink-2);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .branch { color: var(--ink-3); max-width: 170px; overflow: hidden; }
  .cellnum {
    font-size: 11px;
    color: var(--ink-3);
    flex: none;
    width: 52px;
    text-align: right;
  }
  .who { width: 44px; }
  .when { width: 56px; }
  .status { flex: none; width: 92px; justify-content: center; }
  .chev { color: var(--ink-4); }

  @media (max-width: 1080px) {
    .branch, .cellnum:not(.when) { display: none; }
  }
</style>
