<script>
  import { ui, openDeploy, go } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { bytes, millis, rate, shortHash } from '../fmt.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';
  import Spark from '../components/Spark.svelte';

  const app = $derived(store.appByName(ui.appId) ?? store.apps[0] ?? null);
  const appDeploys = $derived(app ? store.deploymentsFor(app.name) : []);
  const current = $derived(
    app?.active ? appDeploys.find((d) => d.id === app.active.deployment_id) ?? appDeploys[0] : appDeploys[0] ?? null
  );
  const am = $derived(app ? store.appMetrics(app.name) : null);

  const LED = { active: 'live', building: 'build', failed: 'fail', sealed: 'cold' };
  const STATUS = { active: 'live', building: 'building', failed: 'failed', sealed: 'sealed' };

  const stateLed = $derived(
    app
      ? app.lifecycle_state === 'building'
        ? 'build'
        : app.lifecycle_state === 'cold'
          ? 'cold'
          : app.name.startsWith('pr-')
            ? 'preview'
            : 'live'
      : 'cold'
  );

  // rollback
  let rollbackOpen = $state(false);
  let rollbackTarget = $state(null);
  let rollbackError = $state('');
  let rollbackBusy = $state(false);

  const rollbackCandidates = $derived(
    app ? appDeploys.filter((d) => d.status === 'sealed' && d.id !== app?.active?.deployment_id) : []
  );

  function askRollback() {
    if (!rollbackCandidates.length) {
      rollbackError = 'No prior sealed deployment is available to roll back to.';
      rollbackOpen = true;
      return;
    }
    rollbackError = '';
    rollbackOpen = true;
    rollbackTarget = rollbackCandidates[0]?.id ?? null;
  }

  async function confirmRollback() {
    if (!app || !rollbackTarget || rollbackBusy) return;
    rollbackBusy = true;
    rollbackError = '';
    const target = appDeploys.find((d) => d.id === rollbackTarget);
    const expected = app.active?.artifact_hash ?? target?.artifact_hash ?? '';
    const r = await store.rollback(app.name, rollbackTarget, expected);
    rollbackBusy = false;
    if (!r.ok) {
      rollbackError = r.error ?? 'Rollback failed';
      return;
    }
    rollbackOpen = false;
    rollbackTarget = null;
  }

  function fmtIdle(ms) {
    if (!ms) return 'pinned warm';
    return `${Math.round(ms / 60000)}m idle`;
  }
</script>

{#if app}
  <div class="page screen-enter">
    <!-- ————— header, boxless ————— -->
    <header class="head">
      <Identicon name={app.name} size={46} />
      <div class="title">
        <div class="row1">
          <h1>{app.name}</h1>
          <span class="led {stateLed}" class:breathe={app.lifecycle_state === 'ready'}></span>
          <span class="pill {app.name.startsWith('pr-') ? 'preview' : 'ghost'}">{app.name.startsWith('pr-') ? 'preview' : 'production'}</span>
        </div>
        <div class="domains num">
          {#each app.domains as d}
            <a href="https://{d}" target="_blank" rel="noopener noreferrer" class="dom"
              >{d} <Icon name="ext" size={11} /></a
            >
            <span class="dot">·</span>
          {/each}
        </div>
      </div>
      <div class="actions">
        {#if app.domains?.length}
          <a class="btn primary" href={`https://${app.domains[0]}`} target="_blank" rel="noopener noreferrer"><Icon name="ext" size={13} />Visit</a>
        {/if}
      </div>
    </header>

    <div class="grid">
      <div class="main">
        <!-- ————— current artifact ————— -->
        <section class="card prod">
          <div class="prodhead">
            <span class="label">{app.name.startsWith('pr-') ? 'Preview' : 'Production'}</span>
            {#if current}
              <span class="pill {LED[current.status] === 'cold' ? 'ghost' : LED[current.status]}">{STATUS[current.status] ?? current.status}</span>
            {:else}
              <span class="pill ghost">no artifact</span>
            {/if}
          </div>
          {#if current}
            <button class="commit" onclick={() => openDeploy(app.name, current.id)}>
              {current.source?.branch ? `${current.source.branch} · ${current.source.commit ?? shortHash(current.source_hash)}` : shortHash(current.source_hash)}
            </button>
            <div class="meta num">
              {current.id} · {relativeTime(current.created_ms)}
              {#if current.artifact_hash}· artifact {shortHash(current.artifact_hash)}{/if}
            </div>
          {:else}
            <p class="commit dim">No active artifact yet.</p>
          {/if}
          <div class="hairline-h"></div>
          <div class="prodfoot">
            <div class="stat">
              <span class="label">p50</span>
              <span class="readout md">{am ? millis(am.p50_ms) : '— ms'}</span>
            </div>
            <div class="stat">
              <span class="label">p99</span>
              <span class="readout md">{am ? millis(am.p99_ms) : '— ms'}</span>
            </div>
            <div class="stat">
              <span class="label">Requests</span>
              <span class="readout md">{am ? rate(am.rps_1m) : '—'}<span class="unit">rps</span></span>
            </div>
            <div class="grow"></div>
            {#if app.name.startsWith('pr-')}
              <span class="pill ghost">preview</span>
            {:else}
              <button class="btn" onclick={askRollback}><Icon name="rollback" size={14} />Roll back</button>
            {/if}
            {#if current}
              <button class="btn" onclick={() => openDeploy(app.name, current.id)}>Build log</button>
            {/if}
          </div>
        </section>

        <!-- ————— deploy timeline ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Deploys</span>
            {#if appDeploys.some((_, i) => i)}
              <span class="histbars"><Spark data={store.appRequestSeries(app.name)} w={120} h={16} color="var(--ink-3)" /></span>
            {/if}
          </div>
          {#if appDeploys.length}
            <div class="rows">
              {#each appDeploys as d (d.id)}
                <button class="drow" onclick={() => openDeploy(app.name, d.id)}>
                  <span class="led {LED[d.status] ?? 'cold'}" class:breathe={d.status === 'building'}></span>
                  <span class="dcommit">{d.source?.branch ? `${d.source.branch} · ${d.source.commit ?? shortHash(d.source_hash)}` : shortHash(d.source_hash)}</span>
                  <span class="chip"><Icon name="branch" size={11} />{d.source?.branch ?? '—'}</span>
                  <span class="dnum num when">{relativeTime(d.created_ms)}</span>
                  <span class="chev"><Icon name="chevR" size={13} /></span>
                </button>
              {/each}
            </div>
          {:else}
            <div class="empty mono">no deployments yet</div>
          {/if}
        </section>
      </div>

      <aside class="side">
        <!-- ————— the cage ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Cage</span>
            {#if app.lifecycle_state === 'ready'}
              <span class="pill live">ready</span>
            {:else if app.lifecycle_state === 'building'}
              <span class="pill build">swapping</span>
            {:else}
              <span class="pill ghost">cold</span>
            {/if}
          </div>
          {#if app.lifecycle_state === 'cold'}
            <div class="coldbox">
              <p>No process. The artifact sleeps on disk — {app.env_keys?.length ?? 0} env keys sealed, route armed.</p>
              <div class="coldstat num">next request revives the cage</div>
            </div>
          {:else}
            <div class="kv">
              <div class="kvrow"><span>p50</span><b class="num">{am ? millis(am.p50_ms) : '—'}</b></div>
              <div class="kvrow"><span>p99</span><b class="num">{am ? millis(am.p99_ms) : '—'}</b></div>
              <div class="kvrow"><span>rps</span><b class="num">{am ? rate(am.rps_1m) : '—'}</b></div>
            </div>
          {/if}
        </section>

        <!-- ————— controls (read-only facts) ————— -->
        <section class="card">
          <div class="cardhead"><span class="label">Controls</span></div>
          <div class="kv">
            <div class="kvrow"><span>Pinned</span><span class="factpill {app.pinned ? 'on' : ''}">{app.pinned ? 'pinned warm' : 'unpinned'}</span></div>
            <div class="kvrow"><span>Egress</span><b class="num">{app.egress ?? '—'}</b></div>
            <div class="kvrow"><span>Idle TTL</span><b class="num">{fmtIdle(app.idle_ttl_ms)}</b></div>
            <div class="kvrow"><span>Memory cap</span><b class="num">{app.memory_max ? bytes(app.memory_max) : '—'}</b></div>
          </div>
        </section>

        <!-- ————— sealed env ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Environment</span>
            <span class="envcount num">{app.env_keys?.length ?? 0} keys</span>
          </div>
          {#if app.env_keys?.length}
            <div class="kv">
              {#each app.env_keys as k (k)}
                <div class="kvrow env"><span class="num">{k}</span><span class="badge">set</span></div>
              {/each}
            </div>
          {:else}
            <div class="empty mono">no env keys</div>
          {/if}
          <div class="sealed"><Icon name="lock" size={11} /> sealed at rest · values never sent to the console</div>
        </section>
      </aside>
    </div>
  </div>

  {#if rollbackOpen}
    <div class="scrim" onclick={(e) => { if (e.target === e.currentTarget) rollbackOpen = false; }} role="presentation">
      <div class="dialog" role="dialog" aria-label="Confirm rollback">
        <p class="eyebrow">CONFIRM ROLLBACK</p>
        <h2>Swap {app.name} to a prior deployment?</h2>
        <p class="dcopy">The active artifact is checked (CAS) before the retained deployment is promoted. No rebuild is started.</p>
        {#if rollbackCandidates.length}
          <div class="targets">
            {#each rollbackCandidates as d (d.id)}
              <label class="target">
                <input type="radio" name="rb" value={d.id} bind:group={rollbackTarget} />
                <span class="led {LED[d.status]}"></span>
                <span class="tid num">{d.id}</span>
                <span class="tart num">{shortHash(d.artifact_hash)}</span>
                <span class="twhen num">{relativeTime(d.created_ms)}</span>
              </label>
            {/each}
          </div>
        {/if}
        {#if rollbackError}<p class="rerr" role="alert">{rollbackError}</p>{/if}
        <div class="dactions">
          <button class="btn" onclick={() => (rollbackOpen = false)}>Cancel</button>
          <button class="btn danger" onclick={confirmRollback} disabled={rollbackBusy || !rollbackTarget}>
            {rollbackBusy ? 'Rolling back…' : 'Confirm rollback'}
          </button>
        </div>
      </div>
    </div>
  {/if}
{:else}
  <div class="page screen-enter">
    <div class="empty mono">no app selected</div>
  </div>
{/if}

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
  .kvrow.env span { font-size: 11px; color: var(--ink-2); }
  .factpill {
    font-family: var(--mono);
    font-size: 10.5px;
    padding: 2px 8px;
    border-radius: 7px;
    color: var(--ink-3);
    background: var(--surface-3);
    border: 1px solid var(--line-2);
  }
  .factpill.on { color: #087a45; background: var(--live-soft); border-color: transparent; }
  .badge {
    font-family: var(--mono);
    font-size: 9.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
    background: var(--surface-3);
    border-radius: 6px;
    padding: 2px 7px;
  }
  .empty {
    padding: 28px 18px;
    font-size: 11px;
    color: var(--ink-4);
    text-align: center;
    letter-spacing: 0.06em;
  }
  .commit.dim { color: var(--ink-3); font-size: 14px; }

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

  /* rollback dialog */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: rgba(12, 15, 20, 0.3);
    backdrop-filter: blur(6px);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 16vh;
  }
  .dialog {
    width: min(520px, calc(100vw - 32px));
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 18px;
    box-shadow: var(--shadow-pop);
    padding: 24px 24px 20px;
  }
  .eyebrow {
    color: var(--amber);
    font: 600 10px var(--mono);
    letter-spacing: 0.15em;
    text-transform: uppercase;
  }
  .dialog h2 { margin-top: 8px; font-size: 17px; letter-spacing: -0.015em; }
  .dcopy { margin-top: 8px; font-size: 12.5px; color: var(--ink-3); line-height: 1.55; }
  .targets { margin-top: 16px; display: flex; flex-direction: column; gap: 6px; max-height: 200px; overflow-y: auto; }
  .target {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--line-2);
    border-radius: 10px;
    cursor: pointer;
    font-size: 12px;
  }
  .target:hover { background: var(--surface-2); }
  .target input { accent-color: var(--cobalt); }
  .target .tid { color: var(--ink); }
  .target .tart { color: var(--ink-3); margin-left: auto; }
  .target .twhen { color: var(--ink-4); }
  .rerr { margin-top: 12px; color: var(--red); font-size: 11.5px; }
  .dactions { display: flex; justify-content: flex-end; gap: 10px; margin-top: 18px; }
  .btn.danger { color: var(--red); border-color: color-mix(in srgb, var(--red) 35%, var(--line)); }
  .btn.danger:hover { background: var(--red-soft); }
</style>
