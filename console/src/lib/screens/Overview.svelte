<script>
  import { store } from '../live.svelte.js';
  import { openApp, go, ui } from '../stores.svelte.js';
  import { relativeTime } from '../time.js';
  import { rate, percent, millis } from '../fmt.js';
  import { eventIcon, eventStyle } from '../events.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';
  import Spark from '../components/Spark.svelte';

  let filter = $state('all');
  const filtered = $derived(
    store.apps.filter((a) =>
      filter === 'all' ? true : filter === 'previews' ? a.name.startsWith('pr-') : !a.name.startsWith('pr-')
    )
  );

  const m = $derived(store.metrics);

  const rps = $derived(m?.totals?.rps_1m ?? null);
  const rpsText = $derived(rps == null ? '—' : rate(rps));
  const sparkData = $derived(m?.series ? m.series.map((b) => b.requests) : []);
  const errRate = $derived(m?.totals?.error_rate_1m ?? null);
  const bootP50 = $derived(m?.totals?.boot_p50_ms ?? null);
  const bootP99 = $derived(m?.totals?.boot_p99_ms ?? null);
  const warm = $derived(store.node?.warm_count ?? store.apps.filter((a) => a.lifecycle_state === 'ready').length);
  const total = $derived(store.apps.length || store.node?.app_count || 0);

  function ledFor(a) {
    if (a.lifecycle_state === 'building') return 'build';
    if (a.lifecycle_state === 'cold') return 'cold';
    if (a.name.startsWith('pr-')) return 'preview';
    return 'live';
  }

  function appRps(a) {
    const am = store.appMetrics(a.name);
    return am?.rps_1m ?? 0;
  }

  const hasMemory = $derived(!!store.node?.memory);
  const usedBytes = $derived(
    store.node?.memory ? store.node.memory.total_bytes - store.node.memory.available_bytes : 0
  );
  const usedPct = $derived(
    store.node?.memory ? (usedBytes / store.node.memory.total_bytes) * 100 : 0
  );

  const recentEvents = $derived(store.events.slice(0, 6));
</script>

<div class="page screen-enter">
  <!-- ————— pulse strip ————— -->
  <section class="pulse card">
    <div class="cell">
      <span class="label">Requests · node</span>
      <div class="row">
        <span class="readout big">{rpsText}<span class="unit">rps</span></span>
        {#if sparkData.length}<Spark data={sparkData} w={92} h={30} />{/if}
      </div>
      <span class="sub num">{m ? `proxy overhead · ${rate(m.totals.p50_ms)} ms p50` : 'collecting…'}</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Revival · p99</span>
      <div class="row">
        <span class="readout big">{bootP99 == null ? '—' : `${Math.round(bootP99)}`}<span class="unit">ms</span></span>
      </div>
      <span class="sub num">{bootP50 == null ? 'collecting…' : `p50 ${Math.round(bootP50)} ms · budget ≤ 150`}</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Error rate · 1m</span>
      <div class="row">
        <span class="readout big">{errRate == null ? '—' : percent(errRate, 2).replace('%', '')}<span class="unit">%</span></span>
      </div>
      <span class="sub num">{m ? `${m.totals.requests_1m.toLocaleString()} req · 1m` : 'collecting…'}</span>
    </div>
    <div class="hairline-v"></div>
    <div class="cell">
      <span class="label">Cages</span>
      <div class="row">
        <span class="readout big"
          >{warm}<span class="unit">warm</span><span class="dim num">&nbsp;/ {total}</span></span
        >
      </div>
      <span class="sub num">{Math.max(0, total - warm)} asleep · disk only</span>
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
        {#each filtered as a (a.name)}
          {@const am = store.appMetrics(a.name)}
          <button class="app card" onclick={() => openApp(a.name)}>
            <div class="top">
              <Identicon name={a.name} size={34} />
              <div class="names">
                <span class="name">{a.name}</span>
                <span class="fw">{a.active ? a.active.engine_version : 'no active artifact'}</span>
              </div>
              <span class="led {ledFor(a)}" class:breathe={a.lifecycle_state === 'ready'}></span>
            </div>
            <span class="domain num">{a.domains?.[0] ?? 'unrouted'}</span>
            <div class="foot">
              <span class="meta num">
                {#if a.lifecycle_state === 'building'}
                  <em class="bmeta">building · {a.active ? 'sealing' : 'no artifact yet'}</em>
                {:else if a.lifecycle_state === 'cold'}
                  cold · revives on next request
                {:else}
                  {rate(am?.rps_1m ?? 0)} rps · {am ? `${millis(am.p50_ms)} p50` : '—'}
                {/if}
              </span>
              {#if store.appRequestSeries(a.name).some((v) => v > 0)}
                <Spark data={store.appRequestSeries(a.name)} w={70} h={20} color="var(--ink-3)" />
              {/if}
            </div>
          </button>
        {/each}

        <button class="app new" onclick={() => { ui.shipOpen = true; ui.shipTab = 'upload'; }}>
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
        {#if recentEvents.length}
          <div class="rows">
            {#each recentEvents as e (e.time_ms + e.type)}
              <button
                type="button"
                class="event event-btn"
                onclick={() => go('observe', { observeAppFilter: e.app ?? '' })}
              >
                <span class="eicon" style={eventStyle(e.type)}>
                  <Icon name={eventIcon(e.type)} size={13} />
                </span>
                <div class="etext">
                  <span class="eapp num">{e.app ?? 'node'}</span>
                  <span class="emsg">{e.message}</span>
                </div>
                <span class="ewhen num">{relativeTime(e.time_ms)}</span>
              </button>
            {/each}
          </div>
        {:else}
          <div class="empty mono">collecting…</div>
        {/if}
      </section>

      <section class="card nodeCard" role="button" tabindex="0" onclick={() => go('node')} onkeydown={(e) => e.key === 'Enter' && go('node')}>
        <div class="cardhead">
          <span class="label">Node</span>
          <span class="nodename num">{store.node?.apps_domain ?? 'cygnus'}</span>
        </div>
        {#if hasMemory}
          <div class="rambar">
            <i style="width:{usedPct}%" class="b-cages" title="used"></i>
          </div>
          <div class="ramlegend num">
            <span>{(usedBytes / (1024 ** 3)).toFixed(1)} / {(store.node.memory.total_bytes / (1024 ** 3)).toFixed(0)} GB</span>
            <span class="dim">{warm} cages warm</span>
          </div>
        {/if}
        <div class="nstats">
          <div class="nrow"><span>Version</span><b class="num">{store.node?.version ?? '—'}</b></div>
          <div class="nrow"><span>Isolation</span><b class="num">{store.node?.isolation ?? '—'}</b></div>
          <div class="nrow"><span>Apps</span><b class="num">{store.node?.app_count ?? store.apps.length}</b></div>
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
  .events .empty {
    padding: 22px 16px;
    font-size: 11px;
    color: var(--ink-4);
    text-align: center;
    letter-spacing: 0.06em;
  }
  .event {
    display: flex;
    gap: 11px;
    padding: 10px 9px;
    align-items: flex-start;
    border-top: none !important;
  }
  .event + .event { border-top: 1px solid var(--line-2) !important; }
  .event-btn {
    width: 100%;
    background: none;
    border: none;
    text-align: left;
    cursor: pointer;
    border-radius: 8px;
  }
  .event-btn:hover { background: var(--surface-2); }
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
