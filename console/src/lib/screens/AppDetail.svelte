<script>
  import { ui, openDeploy, go } from '../stores.svelte.js';
  import { apps, deploys } from '../data.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';
  import Bars from '../components/Bars.svelte';

  const app = $derived(apps.find((a) => a.id === ui.appId) ?? apps[0]);
  const appDeploys = $derived(deploys.filter((d) => d.app === app.id));
  const current = $derived(appDeploys.find((d) => d.status === 'live') ?? appDeploys[0]);

  const LED = { live: 'live', building: 'build', failed: 'fail', preview: 'preview', previous: 'cold' };
  const STATUS = { live: 'production', building: 'building', failed: 'failed', preview: 'preview', previous: 'retained' };

  const stateLed = $derived(
    app.state === 'building' ? 'build' : app.state === 'cold' ? 'cold' : app.env === 'preview' ? 'preview' : 'live'
  );
</script>

<div class="page screen-enter">
  <!-- ————— header, boxless ————— -->
  <header class="head">
    <Identicon name={app.name} size={46} />
    <div class="title">
      <div class="row1">
        <h1>{app.name}</h1>
        <span class="led {stateLed}" class:breathe={app.state === 'ready'}></span>
        <span class="pill {app.env === 'preview' ? 'preview' : 'ghost'}">{app.env}</span>
      </div>
      <div class="domains num">
        <a href="https://{app.domain}" target="_blank" rel="noopener noreferrer" class="dom"
          >{app.domain} <Icon name="ext" size={11} /></a
        >
        {#if app.custom}
          <span class="dot">·</span>
          <a href="https://{app.custom}" target="_blank" rel="noopener noreferrer" class="dom"
            >{app.custom} <Icon name="ext" size={11} /></a
          >
        {/if}
      </div>
    </div>
    <div class="actions">
      <button class="btn" onclick={() => go('observe')}><Icon name="terminal" size={14} />Logs</button>
      <button class="btn primary"><Icon name="ext" size={13} />Visit</button>
      <button class="btn icon"><Icon name="dots" size={15} /></button>
    </div>
  </header>

  <div class="grid">
    <div class="main">
      <!-- ————— current artifact ————— -->
      <section class="card prod">
        <div class="prodhead">
          <span class="label">{app.env === 'preview' ? 'Preview' : 'Production'}</span>
          <span class="pill {LED[current.status] === 'cold' ? 'ghost' : LED[current.status]}"
            >{current.status === 'live' ? 'live' : STATUS[current.status]}</span
          >
        </div>
        <button class="commit" onclick={() => openDeploy(app.id, current.id)}>{current.commit}</button>
        <div class="meta num">
          {current.id} · {current.author} · {current.when}
          {#if current.dur !== '—'}· built in {current.dur}{/if}
        </div>
        <div class="hairline-h"></div>
        <div class="prodfoot">
          <div class="stat">
            <span class="label">Revival</span>
            <span class="readout md">{app.revival}<span class="unit">ms</span></span>
          </div>
          <div class="stat">
            <span class="label">Bundle</span>
            <span class="readout md">{current.size === '—' ? '…' : current.size.replace(' MB', '')}<span class="unit">MB</span></span>
          </div>
          <div class="stat">
            <span class="label">Requests</span>
            <span class="readout md">{app.rps}<span class="unit">rps</span></span>
          </div>
          <div class="grow"></div>
          {#if app.env === 'preview'}
            <button class="btn cobalt"><Icon name="ship" size={13} />Promote</button>
          {:else}
            <button class="btn"><Icon name="rollback" size={14} />Roll back</button>
          {/if}
          <button class="btn" onclick={() => openDeploy(app.id, current.id)}>Build log</button>
        </div>
      </section>

      <!-- ————— deploy timeline ————— -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Deploys</span>
          <span class="histbars"><Bars data={app.history} h={16} /></span>
        </div>
        <div class="rows">
          {#each appDeploys as d (d.id)}
            <button class="drow" onclick={() => openDeploy(app.id, d.id)}>
              <span class="led {LED[d.status]}" class:breathe={d.status === 'building'}></span>
              <span class="dcommit">{d.commit}</span>
              <span class="chip"><Icon name="branch" size={11} />{d.branch}</span>
              <span class="dnum num">{d.author}</span>
              <span class="dnum num">{d.dur}</span>
              <span class="dnum num when">{d.when}</span>
              <span class="chev"><Icon name="chevR" size={13} /></span>
            </button>
          {/each}
        </div>
      </section>
    </div>

    <aside class="side">
      <!-- ————— the cage ————— -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Cage</span>
          {#if app.state === 'ready'}
            <span class="pill live">ready</span>
          {:else if app.state === 'building'}
            <span class="pill build">swapping</span>
          {:else}
            <span class="pill ghost">cold</span>
          {/if}
        </div>
        {#if app.state === 'cold'}
          <div class="coldbox">
            <p>No process. The artifact sleeps on disk — {app.envVars} env vars sealed, route armed.</p>
            <div class="coldstat num">next request revives in ≈{app.revival} ms</div>
            <button class="btn sm"><Icon name="zap" size={12} />Warm up now</button>
          </div>
        {:else}
          <div class="kv">
            <div class="kvrow"><span>Memory</span><b class="num">{app.rss} MB <i>/ {app.memory.split(' ·')[0]}</i></b></div>
            <div class="kvrow"><span>CPU</span><b class="num">{app.cpu}%</b></div>
            <div class="kvrow"><span>Connections</span><b class="num">{app.conns}</b></div>
            <div class="kvrow"><span>Cage uptime</span><b class="num">{app.cageUptime}</b></div>
            <div class="kvrow"><span>Last revival</span><b class="num">{app.revival} ms</b></div>
          </div>
        {/if}
      </section>

      <!-- ————— controls ————— -->
      <section class="card">
        <div class="cardhead"><span class="label">Controls</span></div>
        <div class="kv">
          <div class="kvrow">
            <span>Pinned warm</span>
            <i class="switch" class:on={app.pinned}></i>
          </div>
          <div class="kvrow">
            <span>JIT</span>
            <i class="switch" class:on={app.jit}></i>
          </div>
          <div class="kvrow"><span>Egress</span><b class="num">{app.egress}</b></div>
          <div class="kvrow"><span>Memory cap</span><b class="num">{app.memory}</b></div>
          <div class="kvrow"><span>Idle TTL</span><b class="num">{app.idleTtl}</b></div>
          <div class="kvrow"><span>Engine</span><b class="num">{app.engine}</b></div>
        </div>
      </section>

      <!-- ————— sealed env ————— -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Environment</span>
          <span class="envcount num">{app.envVars} vars</span>
        </div>
        <div class="kv">
          <div class="kvrow env"><span class="num">DATABASE_URL</span><b class="mask">••••••••</b><Icon name="eye" size={13} /></div>
          <div class="kvrow env"><span class="num">STRIPE_SECRET</span><b class="mask">••••••••</b><Icon name="eye" size={13} /></div>
          <div class="kvrow env"><span class="num">SESSION_KEY</span><b class="mask">••••••••</b><Icon name="eye" size={13} /></div>
        </div>
        <div class="sealed"><Icon name="lock" size={11} /> encrypted at rest · XChaCha20-Poly1305</div>
      </section>
    </aside>
  </div>
</div>

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 26px 44px 0;
  }

  .head {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 24px;
  }
  .title { flex: 1; min-width: 0; }
  .row1 {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  h1 {
    font-size: 23px;
    font-weight: 650;
    letter-spacing: -0.02em;
  }
  .domains {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
    font-size: 12px;
  }
  .dom {
    color: var(--ink-3);
    display: inline-flex;
    align-items: center;
    gap: 4px;
    transition: color 0.12s ease;
  }
  .dom:hover { color: var(--cobalt-deep); }
  .dot { color: var(--ink-4); }
  .actions { display: flex; gap: 9px; }

  .grid {
    display: grid;
    grid-template-columns: 1fr 322px;
    gap: 20px;
    align-items: start;
  }
  .main { display: flex; flex-direction: column; gap: 16px; }
  .side { display: flex; flex-direction: column; gap: 16px; }

  /* production card */
  .prod { padding: 20px 22px 16px; }
  .prodhead {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 14px;
  }
  .commit {
    font-size: 17px;
    font-weight: 650;
    letter-spacing: -0.015em;
    text-align: left;
    line-height: 1.35;
    transition: color 0.12s ease;
  }
  .commit:hover { color: var(--cobalt-deep); }
  .meta {
    margin: 7px 0 16px;
    font-size: 11.5px;
    color: var(--ink-3);
  }
  .prodfoot {
    display: flex;
    align-items: flex-end;
    gap: 28px;
    padding-top: 15px;
  }
  .stat { display: flex; flex-direction: column; gap: 6px; }
  .readout.md { font-size: 21px; line-height: 1; }
  .grow { flex: 1; }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
  }
  .histbars { opacity: 0.9; }

  .rows { padding: 4px 8px 8px; }
  .drow {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 13px;
    padding: 11px 10px;
    border-radius: 10px;
    text-align: left;
    transition: background 0.12s ease;
  }
  .drow:hover { background: var(--surface-2); }
  .drow + .drow { border-top: 1px solid var(--line-2); }
  .dcommit {
    flex: 1;
    min-width: 0;
    font-size: 13px;
    color: var(--ink);
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .chip { color: var(--ink-3); max-width: 160px; overflow: hidden; }
  .dnum { font-size: 11px; color: var(--ink-3); width: 44px; text-align: right; flex: none; }
  .when { width: 52px; }
  .chev { color: var(--ink-4); }

  /* side cards */
  .kv { padding: 2px 10px 12px; }
  .kvrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 8.5px 8px;
    font-size: 12.5px;
  }
  .kvrow + .kvrow { border-top: 1px solid var(--line-2); }
  .kvrow span { color: var(--ink-3); }
  .kvrow b { color: var(--ink); font-weight: 500; font-size: 12px; }
  .kvrow b i { font-style: normal; color: var(--ink-4); }
  .kvrow.env b { flex: 1; text-align: right; }
  .kvrow.env span { font-size: 11px; color: var(--ink-2); }
  .mask { color: var(--ink-4) !important; letter-spacing: 0.14em; }
  .kvrow.env :global(svg) { color: var(--ink-4); cursor: pointer; }
  .kvrow.env:hover :global(svg) { color: var(--ink-2); }

  .switch {
    width: 30px;
    height: 18px;
    border-radius: 10px;
    background: var(--ink-4);
    position: relative;
    transition: background 0.15s ease;
    cursor: pointer;
  }
  .switch::after {
    content: '';
    position: absolute;
    top: 2.5px;
    left: 2.5px;
    width: 13px;
    height: 13px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 2px rgba(13, 18, 28, 0.2);
    transition: left 0.15s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .switch.on { background: var(--live); }
  .switch.on::after { left: 14.5px; }

  .coldbox { padding: 4px 18px 16px; }
  .coldbox p {
    font-size: 12.5px;
    color: var(--ink-2);
    line-height: 1.6;
  }
  .coldstat {
    margin: 12px 0;
    padding: 9px 12px;
    background: var(--cobalt-ghost);
    border-radius: 9px;
    font-size: 11.5px;
    color: var(--cobalt-deep);
  }

  .envcount { font-size: 11px; color: var(--ink-3); }
  .sealed {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 10px 18px 14px;
    font-size: 10.5px;
    font-family: var(--mono);
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
  }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
  }
</style>
