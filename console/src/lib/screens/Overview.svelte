<script>
  import { apps, events, node } from '../data.js';
  import { openApp, go, ui } from '../stores.svelte.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';
  import Spark from '../components/Spark.svelte';
  import Bars from '../components/Bars.svelte';

  let filter = $state('all');
  const filtered = $derived(
    apps.filter((a) =>
      filter === 'all' ? true : filter === 'previews' ? a.env === 'preview' : a.env === 'production'
    )
  );

  // static preview request pulse from the fixture dataset
  let rps = $state(1284);
  let sparkData = $state([48, 52, 50, 55, 61, 58, 54, 57, 63, 60, 56, 59, 66, 62, 58, 61, 55, 57, 64, 60, 62, 59, 61, 65]);

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

  function ledFor(a) {
    if (a.state === 'building') return 'build';
    if (a.state === 'cold') return 'cold';
    if (a.env === 'preview') return 'preview';
    return 'live';
  }

  const ramPct = $derived({
    cages: (node.ramCages / node.ram) * 100,
    engines: (node.ramEngines / node.ram) * 100,
    system: (node.ramSystem / node.ram) * 100,
  });
</script>

<div class="page screen-enter">
  <!-- ————— pulse strip ————— -->
  <section class="pulse card">
    <div class="cell">
      <span class="label">Requests · node</span>
      <div class="row">
        <span class="readout big">{rps.toLocaleString()}<span class="unit">rps</span></span>
        <Spark data={sparkData} w={92} h={30} />
      </div>
      <span class="sub num">proxy overhead 0.3 ms p50</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Revival · p99</span>
      <div class="row">
        <span class="readout big">{node.coldStart.p99}<span class="unit">ms</span></span>
      </div>
      <span class="sub num">p50 {node.coldStart.p50} ms · budget ≤ 150</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Error rate · 1h</span>
      <div class="row">
        <span class="readout big">0.03<span class="unit">%</span></span>
      </div>
      <span class="sub num">4xx excluded · 2 of 6.1k</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Cages</span>
      <div class="row">
        <span class="readout big"
          >{node.warm}<span class="unit">warm</span><span class="dim num">&nbsp;/ {node.registered}</span></span
        >
      </div>
      <span class="sub num">{node.registered - node.warm} asleep · disk only</span>
    </div>
  </section>

  <div class="grid">
    <!-- ————— apps ————— -->
    <section class="appsCol">
      <div class="colhead">
        <h2>Apps</h2>
        <span class="count num">{filtered.length}</span>
        <div class="grow"></div>
        <div class="seg">
          <button class:on={filter === 'all'} onclick={() => (filter = 'all')}>All</button>
          <button class:on={filter === 'production'} onclick={() => (filter = 'production')}>Production</button>
          <button class:on={filter === 'previews'} onclick={() => (filter = 'previews')}>Previews</button>
        </div>
      </div>

      <div class="appgrid">
        {#each filtered as a (a.id)}
          <button class="app card" onclick={() => openApp(a.id)}>
            <div class="top">
              <Identicon name={a.name} size={34} />
              <div class="names">
                <span class="name">{a.name}</span>
                <span class="fw">{a.framework}</span>
              </div>
              <span class="led {ledFor(a)}" class:breathe={a.state === 'ready'}></span>
            </div>
            <span class="domain num">{a.domain}</span>
            <div class="foot">
              <span class="meta num">
                {#if a.state === 'building'}
                  <em class="bmeta">building · {a.branch}</em>
                {:else if a.state === 'cold'}
                  cold · revives ≈{a.revival} ms
                {:else}
                  {a.branch} · {a.lastDeploy} · {a.revival} ms
                {/if}
              </span>
              <Bars data={a.history} h={20} />
            </div>
          </button>
        {/each}

        <button class="app new" onclick={() => (ui.shipOpen = true)}>
          <span class="plus"><Icon name="plus" size={16} /></span>
          <span>Ship an app</span>
          <span class="hint num">cygnus deploy</span>
        </button>
      </div>
    </section>

    <!-- ————— side rail ————— -->
    <aside class="side">
      <section class="card events">
        <div class="cardhead">
          <span class="label">Events</span>
          <button class="mini" onclick={() => go('observe')}>View all <Icon name="arrowR" size={12} /></button>
        </div>
        <div class="rows">
          {#each events.slice(0, 6) as e}
            <div class="event">
              <span class="eicon" style="color:{TONE_FG[e.tone]};background:{TONE_BG[e.tone]}">
                <Icon name={EVENT_ICON[e.type]} size={13} />
              </span>
              <div class="etext">
                <span class="eapp num">{e.app}</span>
                <span class="emsg">{e.text}</span>
              </div>
              <span class="ewhen num">{e.when}</span>
            </div>
          {/each}
        </div>
      </section>

      <section class="card nodeCard" role="button" tabindex="0" onclick={() => go('node')} onkeydown={(e) => e.key === 'Enter' && go('node')}>
        <div class="cardhead">
          <span class="label">Node</span>
          <span class="nodename num">swan-01 <span class="led live breathe"></span></span>
        </div>
        <div class="rambar">
          <i style="width:{ramPct.cages}%" class="b-cages" title="warm cages"></i>
          <i style="width:{ramPct.engines}%" class="b-engines" title="engine text"></i>
          <i style="width:{ramPct.system}%" class="b-system" title="system"></i>
        </div>
        <div class="ramlegend num">
          <span>{node.ramUsed} / {node.ram} GB</span>
          <span class="dim">{node.warm} cages warm</span>
        </div>
        <div class="nstats">
          <div class="nrow"><span>Kernel</span><b class="num">{node.kernel} · patched {node.kernelPatched}</b></div>
          <div class="nrow"><span>Engine</span><b class="num">bun 1.2.19 · page-cache shared</b></div>
          <div class="nrow"><span>Uptime</span><b class="num">{node.uptime}</b></div>
        </div>
      </section>
    </aside>
  </div>
</div>

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 20px 44px 0;
  }

  /* pulse */
  .pulse {
    display: flex;
    padding: 24px 10px;
  }
  .cell {
    flex: 1;
    padding: 0 26px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .cell .row {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 12px;
    color: var(--cobalt);
  }
  .readout.big {
    font-size: 33px;
    line-height: 1;
    color: var(--ink);
  }
  .readout .dim {
    font-size: 15px;
    color: var(--ink-3);
    font-weight: 400;
  }
  .sub {
    font-size: 11px;
    color: var(--ink-3);
  }

  /* layout */
  .grid {
    display: grid;
    grid-template-columns: 1fr 336px;
    gap: 22px;
    margin-top: 22px;
    align-items: start;
  }

  .colhead {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 6px 2px 14px;
  }
  h2 {
    font-size: 17px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }
  .count {
    font-size: 11px;
    color: var(--ink-3);
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    padding: 2px 7px;
    border-radius: 7px;
  }
  .grow { flex: 1; }

  /* app cards */
  .appgrid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 14px;
  }
  .app {
    display: flex;
    flex-direction: column;
    gap: 10px;
    padding: 17px 18px 15px;
    text-align: left;
    transition: transform 0.15s cubic-bezier(0.22, 1, 0.36, 1), box-shadow 0.15s ease, border-color 0.15s ease;
  }
  .app:hover {
    transform: translateY(-1.5px);
    box-shadow: var(--shadow-hover);
    border-color: var(--line-strong);
  }
  .app .top {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .names {
    display: flex;
    flex-direction: column;
    gap: 1px;
    flex: 1;
    min-width: 0;
  }
  .name {
    font-size: 14.5px;
    font-weight: 650;
    letter-spacing: -0.01em;
  }
  .fw {
    font-size: 11px;
    color: var(--ink-3);
  }
  .domain {
    font-size: 11.5px;
    color: var(--ink-2);
    background: var(--surface-3);
    border-radius: 7px;
    padding: 4px 8px;
    width: fit-content;
  }
  .foot {
    display: flex;
    align-items: flex-end;
    justify-content: space-between;
    gap: 10px;
    margin-top: 2px;
  }
  .meta {
    font-size: 11px;
    color: var(--ink-3);
  }
  .bmeta {
    font-style: normal;
    color: #a36a06;
  }

  .app.new {
    border: 1.5px dashed var(--line-strong);
    background: transparent;
    box-shadow: none;
    align-items: center;
    justify-content: center;
    gap: 7px;
    min-height: 128px;
    color: var(--ink-2);
    font-size: 13px;
    font-weight: 600;
    border-radius: var(--r-l);
  }
  .app.new:hover {
    border-color: var(--cobalt);
    color: var(--cobalt-deep);
    background: var(--cobalt-ghost);
    transform: none;
  }
  .plus {
    width: 30px;
    height: 30px;
    border-radius: 10px;
    background: var(--surface);
    border: 1px solid var(--line);
    display: grid;
    place-items: center;
  }
  .app.new .hint {
    font-size: 10.5px;
    color: var(--ink-4);
  }

  /* side */
  .side {
    display: flex;
    flex-direction: column;
    gap: 16px;
    position: sticky;
    top: 20px;
  }
  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 17px 11px;
  }
  .mini {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-size: 11.5px;
    font-weight: 600;
    color: var(--ink-3);
    transition: color 0.12s ease;
  }
  .mini:hover { color: var(--ink); }

  .events .rows { padding: 0 8px 8px; }
  .event {
    display: flex;
    gap: 11px;
    padding: 10px 9px;
    align-items: flex-start;
    border-top: none !important;
  }
  .event + .event { border-top: 1px solid var(--line-2) !important; }
  .eicon {
    width: 26px;
    height: 26px;
    border-radius: 9px;
    display: grid;
    place-items: center;
    flex: none;
    margin-top: 1px;
  }
  .etext {
    display: flex;
    flex-direction: column;
    gap: 1px;
    min-width: 0;
  }
  .eapp {
    font-size: 11px;
    color: var(--ink);
    font-weight: 600;
  }
  .emsg {
    font-size: 11.5px;
    color: var(--ink-3);
    line-height: 1.45;
  }
  .ewhen {
    margin-left: auto;
    font-size: 10.5px;
    color: var(--ink-4);
    flex: none;
  }

  /* node card */
  .nodeCard { cursor: pointer; transition: border-color 0.14s ease, box-shadow 0.14s ease; }
  .nodeCard:hover { border-color: var(--line-strong); box-shadow: var(--shadow-hover); }
  .nodename {
    font-size: 12px;
    color: var(--ink);
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }
  .rambar {
    display: flex;
    gap: 2px;
    height: 9px;
    border-radius: 5px;
    background: var(--surface-3);
    overflow: hidden;
    margin: 2px 17px 0;
  }
  .rambar i { display: block; border-radius: 2px; }
  .b-cages { background: var(--cobalt); }
  .b-engines { background: var(--violet); }
  .b-system { background: var(--ink-4); }
  .ramlegend {
    display: flex;
    justify-content: space-between;
    font-size: 10.5px;
    color: var(--ink-2);
    margin: 8px 17px 0;
  }
  .ramlegend .dim { color: var(--ink-3); }
  .nstats {
    margin: 12px 9px 9px;
    border-top: 1px solid var(--line-2);
    padding-top: 5px;
  }
  .nrow {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 7px 8px;
    font-size: 11.5px;
  }
  .nrow span { color: var(--ink-3); }
  .nrow b { color: var(--ink); font-weight: 500; font-size: 11px; }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
    .appgrid { grid-template-columns: 1fr; }
    .pulse { flex-wrap: wrap; gap: 18px; }
    .hairline-v { display: none; }
  }
</style>
