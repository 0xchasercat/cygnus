<script>
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { shortHash } from '../fmt.js';
  import { openDeploy } from '../stores.svelte.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';

  let filter = $state('all');

  const filtered = $derived(
    store.deployments.filter((d) => {
      if (filter === 'all') return true;
      if (filter === 'active') return d.status === 'active';
      if (filter === 'building') return d.status === 'building';
      if (filter === 'failed') return d.status === 'failed';
      return true;
    })
  );

  const LED = { active: 'live', building: 'build', failed: 'fail', sealed: 'cold' };
  const STATUS = { active: 'live', building: 'building', failed: 'failed', sealed: 'sealed' };

  // Derive a "branch · sha7" source label when a github job references this deploy.
  function sourceLabel(d) {
    if (d.source?.kind === 'github' && d.source.branch) {
      return `${d.source.branch} · ${d.source.commit ?? shortHash(d.source_hash).slice(0, 7)}`;
    }
    if (d.source?.kind === 'upload') return 'folder upload';
    if (d.source?.kind === 'cli') return 'cli';
    const job = store.github.jobs.find((j) => j.deployment_id === d.id);
    if (job) return `${job.branch ?? '—'} · ${shortHash(job.sha).slice(0, 7)}`;
    return '—';
  }
</script>

<div class="page screen-enter">
  <div class="head">
    <div>
      <h1>Deploys</h1>
    </div>
    <div class="seg">
      <button class:on={filter === 'all'} onclick={() => (filter = 'all')}>All</button>
      <button class:on={filter === 'active'} onclick={() => (filter = 'active')}>Active</button>
      <button class:on={filter === 'building'} onclick={() => (filter = 'building')}>Building</button>
      <button class:on={filter === 'failed'} onclick={() => (filter = 'failed')}>Failed</button>
    </div>
  </div>

  <section class="card">
    {#if filtered.length}
      <div class="rows">
        {#each filtered as d (d.id)}
          <button class="row" onclick={() => openDeploy(d.app, d.id)}>
            <span class="led {LED[d.status]}" class:breathe={d.status === 'building'}></span>
            <span class="appcell">
              <Identicon name={d.app} size={22} />
              <span class="appname">{d.app}</span>
            </span>
            <span class="commit num">{d.id}</span>
            <span class="chip branch"><Icon name="branch" size={11} />{sourceLabel(d)}</span>
            <span class="cellnum num">{d.engine_version ?? '—'}</span>
            <span class="cellnum num">{shortHash(d.artifact_hash)}</span>
            <span class="cellnum num when">{d.created_ms ? relativeTime(d.created_ms) : '—'}</span>
            <span class="status pill {LED[d.status] === 'cold' ? 'ghost' : LED[d.status]}">{STATUS[d.status] ?? d.status}</span>
            <span class="chev"><Icon name="chevR" size={13} /></span>
          </button>
        {/each}
      </div>
    {:else}
      <div class="empty mono">no deployments yet</div>
    {/if}
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

  .rows { padding: 6px; }
  .empty {
    padding: 48px 18px;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }
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
