<script>
  import { latency, nextRequest, events } from '../data.js';
  import Chart from '../components/Chart.svelte';
  import Icon from '../components/Icon.svelte';

  let range = $state('1h');
  let tab = $state('requests');

  let reqs = $state(Array.from({ length: 14 }, () => nextRequest()).reverse());

  $effect(() => {
    const t = setInterval(() => {
      reqs = [nextRequest(), ...reqs].slice(0, 22);
    }, 1600);
    return () => clearInterval(t);
  });

  const EVENT_ICON = {
    deploy: 'ship',
    revival: 'zap',
    scale0: 'clock',
    oom: 'node',
    seccomp: 'lock',
    cert: 'globe',
  };
  const TONE_FG = {
    live: '#087a45',
    cobalt: 'var(--cobalt-deep)',
    ghost: 'var(--ink-3)',
    amber: '#a36a06',
    red: '#b02c23',
  };
  const TONE_BG = {
    live: 'var(--live-soft)',
    cobalt: 'var(--cobalt-ghost)',
    ghost: 'var(--surface-3)',
    amber: 'var(--amber-soft)',
    red: 'var(--red-soft)',
  };
</script>

<div class="page screen-enter">
  <div class="head">
    <div>
      <h1>Observe</h1>
      <p class="sub">Measured at the router — none of it self-reported by cages.</p>
    </div>
    <div class="seg">
      <button class:on={range === '1h'} onclick={() => (range = '1h')}>1h</button>
      <button class:on={range === '24h'} onclick={() => (range = '24h')}>24h</button>
      <button class:on={range === '7d'} onclick={() => (range = '7d')}>7d</button>
    </div>
  </div>

  <section class="card">
    <div class="cardhead">
      <span class="label">Latency · all apps</span>
      <div class="legend num">
        <span><i class="sw p50"></i>p50</span>
        <span><i class="sw p99"></i>p99</span>
        <span class="dim">router adds 0.3 ms</span>
      </div>
    </div>
    <div class="chartwrap">
      <Chart series={latency} h={225} />
    </div>
  </section>

  <section class="card stream">
    <div class="cardhead">
      <div class="seg">
        <button class:on={tab === 'requests'} onclick={() => (tab = 'requests')}>Requests</button>
        <button class:on={tab === 'events'} onclick={() => (tab = 'events')}>Events</button>
      </div>
      {#if tab === 'requests'}
        <span class="livehint num"><span class="led live breathe"></span>streaming</span>
      {/if}
    </div>

    {#if tab === 'requests'}
      <div class="rows">
        {#each reqs as r (r.id)}
          <div class="req">
            <span class="time num">{r.time}</span>
            <span class="method num">{r.method}</span>
            <span class="path num">{r.path}</span>
            <span class="appchip num">{r.app}</span>
            {#if r.cold}
              <span class="pill cobalt">revived · {r.dur} ms</span>
            {/if}
            <span class="status num" class:err={r.status >= 500}>{r.status}</span>
            <span class="dur num">{r.dur} ms</span>
          </div>
        {/each}
      </div>
    {:else}
      <div class="rows">
        {#each events as e}
          <div class="event">
            <span class="eicon" style="color:{TONE_FG[e.tone]};background:{TONE_BG[e.tone]}">
              <Icon name={EVENT_ICON[e.type]} size={14} />
            </span>
            <span class="eapp num">{e.app}</span>
            <span class="emsg">{e.text}</span>
            <span class="ewhen num">{e.when}</span>
          </div>
        {/each}
      </div>
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
