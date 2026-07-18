<script>
  import { millis } from '../fmt.js';
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { eventIcon, eventStyle } from '../events.js';
  import Chart from '../components/Chart.svelte';
  import Icon from '../components/Icon.svelte';

  let tab = $state('requests');

  const series = $derived(
    store.metrics?.series ? store.metrics.series.map((b) => [b.p50_ms, b.p99_ms]) : []
  );

  const reqs = $derived(store.requests.slice(0, 200));
  const events = $derived(store.events);

  function reqTime(r) {
    const d = new Date(r.time_ms ?? 0);
    const hh = String(d.getHours()).padStart(2, '0');
    const mm = String(d.getMinutes()).padStart(2, '0');
    const ss = String(d.getSeconds()).padStart(2, '0');
    return `${hh}:${mm}:${ss}`;
  }
</script>

<div class="page screen-enter">
  <div class="head">
    <div>
      <h1>Observe</h1>
      <p class="sub">Measured at the router — none of it self-reported by cages.</p>
    </div>
    <span class="chip">last 60 min · in-memory</span>
  </div>

  <section class="card">
    <div class="cardhead">
      <span class="label">Latency · all apps</span>
      <div class="legend num">
        <span><i class="sw p50"></i>p50</span>
        <span><i class="sw p99"></i>p99</span>
        {#if store.metrics}<span class="dim">{millis(store.metrics.totals.p50_ms)} / {millis(store.metrics.totals.p99_ms)}</span>{/if}
      </div>
    </div>
    <div class="chartwrap">
      {#if series.length}
        <Chart {series} h={225} />
      {:else}
        <div class="empty mono">collecting…</div>
      {/if}
    </div>
  </section>

  <section class="card stream">
    <div class="cardhead">
      <div class="seg">
        <button class:on={tab === 'requests'} onclick={() => (tab = 'requests')}>Requests</button>
        <button class:on={tab === 'events'} onclick={() => (tab = 'events')}>Events</button>
      </div>
      {#if tab === 'requests'}
        <span class="livehint num"><span class="led {store.mode === 'live' ? 'live' : 'preview'}"></span>{store.mode === 'live' ? 'live' : 'preview'} · newest first</span>
      {/if}
    </div>

    {#if tab === 'requests'}
      {#if reqs.length}
        <div class="rows">
          {#each reqs as r (r.request_id)}
            <div class="req">
              <span class="time num">{reqTime(r)}</span>
              <span class="method num">{r.method}</span>
              <span class="path num">{r.path}</span>
              <span class="appchip num">{r.app}</span>
              {#if r.cold}
                <span class="pill cobalt">revived · {r.duration_ms} ms</span>
              {/if}
              <span class="status num" class:err={r.status >= 500}>{r.status}</span>
              <span class="dur num">{r.duration_ms} ms</span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="empty mono">collecting…</div>
      {/if}
    {:else}
      {#if events.length}
        <div class="rows">
          {#each events as e (e.time_ms + e.type)}
            <div class="event">
              <span class="eicon" style={eventStyle(e.type)}>
                <Icon name={eventIcon(e.type)} size={14} />
              </span>
              <span class="eapp num">{e.app ?? 'node'}</span>
              <span class="emsg">{e.message}</span>
              <span class="ewhen num">{relativeTime(e.time_ms)}</span>
            </div>
          {/each}
        </div>
      {:else}
        <div class="empty mono">no events yet</div>
      {/if}
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
  }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
  }
  .legend {
    display: flex;
    gap: 16px;
    font-size: 11px;
    color: var(--ink-2);
    align-items: center;
  }
  .legend span { display: inline-flex; align-items: center; gap: 6px; }
  .legend .dim { color: var(--ink-4); }
  .sw { width: 14px; height: 0; border-top: 2px solid; border-radius: 2px; display: inline-block; }
  .sw.p50 { border-color: var(--cobalt); }
  .sw.p99 { border-color: var(--ink-4); border-top-style: dashed; }
  .chartwrap { padding: 4px 14px 14px; }
  .chartwrap .empty {
    padding: 60px 0;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }
  .stream .empty {
    padding: 40px 0;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }

  .stream { margin-top: 18px; }
  .livehint {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-size: 11px;
    color: var(--ink-3);
  }

  .rows { padding: 4px 10px 10px; }
  .req {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 8.5px 10px;
    border-radius: 9px;
    animation: req-in 0.3s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  .req + .req { border-top: 1px solid var(--line-2); }
  @keyframes req-in {
    from { opacity: 0; transform: translateY(-4px); }
  }
  .time { font-size: 11px; color: var(--ink-4); width: 60px; flex: none; }
  .method { font-size: 11px; font-weight: 600; color: var(--ink); width: 48px; flex: none; }
  .path {
    font-size: 12px;
    color: var(--ink-2);
    flex: 1;
    min-width: 0;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .appchip {
    font-size: 10.5px;
    color: var(--ink-3);
    background: var(--surface-3);
    padding: 3px 8px;
    border-radius: 7px;
    flex: none;
  }
  .status { font-size: 11.5px; color: var(--ink-3); width: 34px; text-align: right; flex: none; }
  .status.err { color: var(--red); font-weight: 600; }
  .dur { font-size: 11.5px; color: var(--ink); width: 58px; text-align: right; flex: none; }

  .event {
    display: flex;
    align-items: center;
    gap: 13px;
    padding: 10px;
  }
  .event + .event { border-top: 1px solid var(--line-2); }
  .eicon {
    width: 28px;
    height: 28px;
    border-radius: 9px;
    display: grid;
    place-items: center;
    flex: none;
  }
  .eapp { font-size: 11.5px; font-weight: 600; width: 130px; flex: none; }
  .emsg { font-size: 12.5px; color: var(--ink-2); flex: 1; }
  .ewhen { font-size: 11px; color: var(--ink-4); }
</style>
