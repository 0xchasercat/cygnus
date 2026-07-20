<script>
  import { store } from '../live.svelte.js';
  import { previewEgress } from '../fixtures.js';
  import { uptime, relativeTime } from '../time.js';
  import { bytes, millis, phaseLabel } from '../fmt.js';
  import Anatomy from '../components/Anatomy.svelte';
  import Constellation from '../components/Constellation.svelte';
  import Icon from '../components/Icon.svelte';

  const node = $derived(store.node);

  const hasMemory = $derived(!!node?.memory && Number(node.memory.total_bytes) > 0);
  const usedBytes = $derived(hasMemory ? node.memory.total_bytes - node.memory.available_bytes : 0);
  const usedPct = $derived(hasMemory ? (usedBytes / node.memory.total_bytes) * 100 : 0);
  const totalGb = $derived(hasMemory ? node.memory.total_bytes / (1024 ** 3) : 0);

  const bootPhases = $derived(
    store.metrics?.boot_phases?.phases
      ? store.metrics.boot_phases.phases.map((p) => ({ name: phaseLabel(p.name), ms: p.p50_ms, hot: false }))
      : []
  );
  // On portable (macOS) cages the isolation stages are genuinely zero —
  // namespaces/cgroup/network/mounts/seccomp don't run. Keep them visible but
  // mark the real bottleneck so the anatomy isn't a wall of 0.0 ms.
  const hotPhase = $derived(bootPhases.length ? bootPhases.reduce((a, b) => (b.ms > a.ms ? b : a)) : null);
  const anatomyPhases = $derived(bootPhases.map((p) => ({ ...p, hot: hotPhase && p.name === hotPhase.name && p.ms > 0 })));

  // Egress has no live source — render the fixture only in preview.
  const egress = $derived(store.mode === 'preview' ? previewEgress : null);
  const maxEgress = $derived(egress ? Math.max(...egress.top.map((t) => t.gb)) : 0);

  function certLed(c) {
    return c.ok ? 'live' : 'build';
  }
  function certExpiry(c) {
    if (!c.expires_unix) return c.ok ? 'ok' : 'pending';
    const ms = c.expires_unix * 1000;
    return `renews ${relativeTime(ms)}`;
  }
  function shortSha(sha) {
    if (!sha) return '—';
    return sha.slice(0, 8);
  }
</script>

<div class="page screen-enter">
  <header class="head">
    <div>
      <div class="row1">
        <h1>{node?.apps_domain ?? 'cygnus'}</h1>
        <span class="led live breathe"></span>
      </div>
      <p class="sub num">{node?.version ?? 'cygnus dev'}</p>
      <div class="hostchips">
        {#if node?.version}<span class="chip">version <b>{node.version}</b></span>{/if}
        {#if node?.isolation}<span class="chip">{node.isolation}</span>{/if}
        {#if node?.uptime_seconds}<span class="chip">uptime <b>{uptime(node.uptime_seconds)}</b></span>{/if}
        {#if node?.listen}<span class="chip">listen <b>{node.listen}</b></span>{/if}
        {#if node?.https_listen}<span class="chip">https <b>{node.https_listen}</b></span>{/if}
      </div>
    </div>
    <div class="constellation">
      <Constellation w={250} opacity={0.85} />
    </div>
  </header>

  <div class="grid">
    <!-- ————— identity ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Identity</span></div>
      <div class="pad">
        <div class="kv">
          <div class="kvrow"><span>Apps domain</span><b class="num">{node?.apps_domain ?? '—'}</b></div>
          <div class="kvrow"><span>Listener</span><b class="num">{node?.listen ?? '—'}</b></div>
          {#if node?.https_listen}<div class="kvrow"><span>HTTPS</span><b class="num">{node.https_listen}</b></div>{/if}
          {#if node?.isolation}<div class="kvrow"><span>Isolation</span><b class="num">{node.isolation}</b></div>{/if}
          {#if node?.uptime_seconds}<div class="kvrow"><span>Uptime</span><b class="num">{uptime(node.uptime_seconds)}</b></div>{/if}
          <div class="kvrow"><span>Apps</span><b class="num">{node?.app_count ?? store.apps.length}</b></div>
          {#if node?.warm_count != null}<div class="kvrow"><span>Warm</span><b class="num">{node.warm_count}</b></div>{/if}
        </div>
      </div>
    </section>

    <!-- ————— memory density ————— -->
    <section class="card">
      <div class="cardhead">
        <span class="label">Memory · density</span>
        {#if hasMemory}<span class="hint num">{bytes(usedBytes)} / {bytes(node.memory.total_bytes)}</span>{/if}
      </div>
      <div class="pad">
        {#if hasMemory}
          <div class="rambar">
            <i style="width:{usedPct}%" class="b-cages"></i>
          </div>
          <div class="ramlegend">
            <span><i class="dot b-cages"></i>used <b class="num">{bytes(usedBytes)}</b></span>
            <span><i class="dot b-free"></i>free <b class="num">{bytes(node.memory.available_bytes)}</b></span>
          </div>
          <div class="counts">
            <div class="count"><span class="readout md">{node.warm_count ?? '—'}</span><span class="label">warm</span></div>
            <div class="hairline-v"></div>
            <div class="count"><span class="readout md">{node.app_count ?? store.apps.length}</span><span class="label">apps</span></div>
          </div>
          <p class="axiom">Density is bounded by concurrent-active apps, not registered apps.</p>
        {:else}
          <div class="counts">
            <div class="count"><span class="readout md">{node?.app_count ?? store.apps.length}</span><span class="label">apps</span></div>
            <div class="hairline-v"></div>
            <div class="count"><span class="readout md">{node?.warm_count ?? '—'}</span><span class="label">warm</span></div>
          </div>
        {/if}
      </div>
    </section>

    <!-- ————— revival anatomy ————— -->
    <section class="card">
      <div class="cardhead">
        <span class="label">Revival budget</span>
        {#if store.metrics?.totals}
          <span class="hint num">p50 <b>{millis(store.metrics.totals.boot_p50_ms)}</b> · p99 <b>{millis(store.metrics.totals.boot_p99_ms)}</b></span>
        {/if}
      </div>
      <div class="pad">
        {#if anatomyPhases.length}
          <Anatomy phases={anatomyPhases} />
          {#if hotPhase && (hotPhase.name === 'socket ready' || hotPhase.name.includes('socket'))}
            <p class="axiom" style="margin-top:12px">Most of revival is runtime init until the readiness socket accepts — isolation stages are free on this host.</p>
          {/if}
        {:else}
          <div class="empty mono">no boots sampled yet</div>
        {/if}
      </div>
    </section>

    <!-- ————— engines ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Engines · page-cache shared</span></div>
      {#if node?.engines?.length}
        <div class="rows pad0">
          {#each node.engines as e (e.version)}
            <div class="engine">
              <span class="ename num">{e.version}</span>
              {#if e.default}<span class="pill cobalt">default</span>{/if}
              <span class="grow"></span>
              <span class="emeta num">{shortSha(e.sha256)} · {e.apps ?? 0} apps</span>
            </div>
          {/each}
        </div>
        <div class="foot num">one text copy per resident version · unreferenced engines are GC'd</div>
      {:else}
        <div class="empty mono">no engines reported</div>
      {/if}
    </section>

    <!-- ————— certificates ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Certificates · ACME</span></div>
      {#if node?.certificates?.length}
        <div class="rows pad0">
          {#each node.certificates as c (c.domain)}
            <div class="cert">
              <span class="led {certLed(c)}"></span>
              <span class="cdomain num">{c.domain}</span>
              <span class="ckind num">{c.kind}</span>
              <span class="grow"></span>
              <span class="crenew num">{certExpiry(c)}</span>
            </div>
          {/each}
        </div>
        <div class="foot num">keys never enter a cage · hot-loaded into rustls</div>
      {:else}
        <div class="empty mono">no certificates reported</div>
      {/if}
    </section>

    <!-- ————— egress (preview only — no live source) ————— -->
    {#if egress}
      <section class="card">
        <div class="cardhead">
          <span class="label">Egress · nftables per cage</span>
          <span class="hint num">{egress.today} today · {egress.conns} conns</span>
        </div>
        <div class="pad">
          <div class="modes">
            <span class="chip">public <b>{egress.modes.public}</b></span>
            <span class="chip">restricted <b>{egress.modes.restricted}</b></span>
            <span class="chip">none <b>{egress.modes.none}</b></span>
            <span class="chip">open <b>{egress.modes.open}</b></span>
          </div>
          <div class="topapps">
            {#each egress.top as t}
              <div class="tapp">
                <span class="tname num">{t.app}</span>
                <span class="tbar"><i style="width:{(t.gb / maxEgress) * 100}%"></i></span>
                <span class="tgb num">{t.gb} GB</span>
              </div>
            {/each}
          </div>
        </div>
      </section>
    {/if}

    <!-- ————— break-glass ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Break-glass</span></div>
      <div class="pad">
        <p class="bgtext">
          If this console ever bricks itself, the node doesn't care.
          <code>cygnus</code> talks to the daemon over a root-only socket, past everything.
        </p>
        <div class="code num">
          <span class="p">$</span> cygnus --admin-socket /run/cygnus/admin.sock status
        </div>
      </div>
    </section>
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
    justify-content: space-between;
    align-items: flex-start;
    gap: 30px;
    margin-bottom: 22px;
  }
  .row1 { display: flex; align-items: center; gap: 13px; }
  h1 {
    font-size: 24px;
    font-weight: 650;
    letter-spacing: -0.02em;
    font-family: var(--mono);
  }
  .sub {
    margin-top: 6px;
    font-size: 11.5px;
    color: var(--ink-3);
  }
  .hostchips {
    display: flex;
    gap: 8px;
    margin-top: 14px;
    flex-wrap: wrap;
  }
  .constellation { flex: none; margin-top: -8px; }

  .grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 18px;
    align-items: start;
  }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
    gap: 10px;
  }
  .hint { font-size: 11px; color: var(--ink-3); }
  .hint b { color: var(--ink); font-weight: 500; }
  .pad { padding: 4px 18px 18px; }
  .pad0 { padding: 0 10px 6px; }

  .kv { padding: 2px 4px 8px; }
  .kvrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 9px 4px;
    font-size: 12.5px;
  }
  .kvrow + .kvrow { border-top: 1px solid var(--line-2); }
  .kvrow span { color: var(--ink-3); }
  .kvrow b { color: var(--ink); font-weight: 500; font-size: 12px; }
  .empty {
    padding: 36px 18px;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }

  /* density */
  .rambar {
    display: flex;
    gap: 2px;
    height: 13px;
    border-radius: 7px;
    background: var(--surface-3);
    overflow: hidden;
  }
  .rambar i { display: block; border-radius: 3px; }
  .b-cages { background: var(--cobalt); }
  .b-engines { background: var(--violet); }
  .b-system { background: var(--ink-4); }
  .b-free { background: var(--line-2); }
  .ramlegend {
    display: flex;
    gap: 16px;
    flex-wrap: wrap;
    margin-top: 12px;
    font-size: 11px;
    color: var(--ink-3);
  }
  .ramlegend span { display: inline-flex; align-items: center; gap: 6px; }
  .ramlegend b { color: var(--ink); font-weight: 500; }
  .dot { width: 8px; height: 8px; border-radius: 3px; display: inline-block; }
  .counts {
    display: flex;
    gap: 24px;
    margin-top: 18px;
    padding-top: 16px;
    border-top: 1px solid var(--line-2);
  }
  .count { display: flex; flex-direction: column; gap: 5px; }
  .readout.md { font-size: 24px; line-height: 1; }
  .axiom {
    margin-top: 16px;
    font-size: 11.5px;
    font-style: italic;
    color: var(--ink-3);
  }

  /* engines / certs */
  .engine, .cert {
    display: flex;
    align-items: center;
    gap: 11px;
    padding: 11px 10px;
  }
  .engine + .engine, .cert + .cert { border-top: 1px solid var(--line-2); }
  .ename { font-size: 12.5px; font-weight: 600; }
  .emeta, .ckind, .crenew { font-size: 11px; color: var(--ink-3); }
  .cdomain { font-size: 12px; font-weight: 500; }
  .grow { flex: 1; }
  .foot {
    padding: 10px 18px 14px;
    font-size: 10.5px;
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
  }

  /* egress */
  .modes { display: flex; gap: 8px; flex-wrap: wrap; }
  .topapps { margin-top: 16px; display: flex; flex-direction: column; gap: 9px; }
  .tapp { display: flex; align-items: center; gap: 12px; }
  .tname { width: 90px; font-size: 11.5px; color: var(--ink-2); flex: none; }
  .tbar {
    flex: 1;
    height: 7px;
    background: var(--surface-3);
    border-radius: 4px;
    overflow: hidden;
  }
  .tbar i { display: block; height: 100%; background: var(--cobalt); border-radius: 4px; opacity: 0.75; }
  .tgb { width: 52px; text-align: right; font-size: 11px; color: var(--ink); }

  /* break-glass */
  .bgtext {
    font-size: 12.5px;
    line-height: 1.65;
    color: var(--ink-2);
  }
  .bgtext code {
    font-family: var(--mono);
    font-size: 11.5px;
    background: var(--surface-3);
    padding: 1.5px 6px;
    border-radius: 6px;
  }
  .code {
    margin-top: 13px;
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: 10px;
    padding: 11px 14px;
    font-size: 12px;
    color: var(--ink);
  }
  .code .p { color: var(--ink-4); margin-right: 8px; }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
    .constellation { display: none; }
  }
</style>
