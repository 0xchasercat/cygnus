<script>
  import { ui } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { shortHash, millis, phaseLabel } from '../fmt.js';
  import Icon from '../components/Icon.svelte';
  import Terminal from '../components/Terminal.svelte';
  import Anatomy from '../components/Anatomy.svelte';

  const app = $derived(
    store.appByName(ui.appId) ??
      (store.deploymentById(ui.deployId)?.app
        ? store.appByName(store.deploymentById(ui.deployId).app)
        : null) ??
      null,
  );
  const deploy = $derived(
    store.deploymentById(ui.deployId) ??
      (app ? store.deploymentsFor(app.name)[0] : null) ??
      store.deployments[0] ??
      null,
  );

  const LED = { active: 'live', building: 'build', failed: 'fail', sealed: 'cold' };
  const STATUS = { active: 'live', building: 'building', failed: 'failed', sealed: 'sealed' };

  // Vercel-like pipeline: source intake → install → compile → activate.
  const STEPS = [
    { id: 'source', label: 'Source' },
    { id: 'install', label: 'Install' },
    { id: 'build', label: 'Build' },
    { id: 'activate', label: 'Ready' },
  ];

  // Infer the furthest completed step from live log text when possible.
  function inferPhaseFromLogs(text) {
    const t = text.toLowerCase();
    if (
      t.includes('[build] completed') ||
      t.includes('deployed') ||
      t.includes('✔ done') ||
      t.includes('wrote site')
    ) {
      return 3;
    }
    if (
      t.includes('[build] starting') ||
      t.includes('vite') ||
      t.includes('building') ||
      t.includes('[build] static')
    ) {
      return 2;
    }
    if (
      t.includes('[install]') ||
      t.includes('bun install') ||
      t.includes('resolving dependencies') ||
      t.includes('packages installed')
    ) {
      return 1;
    }
    if (t.includes('[detect]') || t.includes('starting frozen') || t.length > 0) {
      return 0;
    }
    return -1;
  }

  let stream = $state('combined'); // combined | stdout | stderr
  let lines = $state([]);
  let logMissing = $state(false);
  let logText = $state(''); // raw joined text for phase inference
  let copied = $state(false);
  let nowMs = $state(Date.now());

  // Wall clock for elapsed timer while building.
  $effect(() => {
    if (!deploy || deploy.status !== 'building') return;
    nowMs = Date.now();
    const timer = setInterval(() => {
      nowMs = Date.now();
    }, 250);
    return () => clearInterval(timer);
  });

  const elapsedMs = $derived.by(() => {
    if (!deploy?.created_ms) return null;
    const end =
      deploy.status === 'building'
        ? nowMs
        : deploy.activated_ms ?? deploy.finished_ms ?? deploy.updated_ms ?? nowMs;
    return Math.max(0, end - deploy.created_ms);
  });

  function formatElapsed(ms) {
    if (ms == null) return '—';
    const total = Math.floor(ms / 1000);
    const m = Math.floor(total / 60);
    const s = total % 60;
    if (m <= 0) return `${s}s`;
    return `${m}m ${String(s).padStart(2, '0')}s`;
  }

  const logPhase = $derived(inferPhaseFromLogs(logText));

  const stepState = $derived.by(() => {
    if (!deploy) return ['todo', 'todo', 'todo', 'todo'];
    if (deploy.status === 'failed') {
      // Mark the furthest inferred step as failed; default to install/build.
      const failAt = Math.max(1, Math.min(2, logPhase < 0 ? 1 : logPhase));
      return STEPS.map((_, i) => {
        if (i < failAt) return 'done';
        if (i === failAt) return 'fail';
        return 'todo';
      });
    }
    if (deploy.status === 'active' || deploy.status === 'sealed') {
      return ['done', 'done', 'done', 'done'];
    }
    // building
    const at = logPhase < 0 ? 0 : Math.min(2, logPhase);
    return STEPS.map((_, i) => {
      if (i < at) return 'done';
      if (i === at) return 'now';
      return 'todo';
    });
  });

  function classifyLine(text) {
    if (/^\$/u.test(text) || /^\[(detect|install|build)\]/u.test(text)) return 'head';
    if (/error|failed|ERR_/iu.test(text)) return 'err';
    if (/warn/iu.test(text)) return 'dim';
    if (/✓|✔|completed|done|deployed|packages installed/iu.test(text)) return 'ok';
    return 'text';
  }

  function toTerminalLines(text, tSec = '') {
    const raw = text.length ? text.split('\n') : [];
    // Keep trailing empty only if it is the sole content; otherwise drop blanks.
    const kept = raw.filter((l, i) => l.length > 0 || (i === raw.length - 1 && raw.length === 1));
    return kept.map((t) => ({ t: tSec, kind: classifyLine(t), text: t }));
  }

  async function pullOne(deployId, streamName, offset) {
    const r = await store.fetchDeploymentLog(deployId, streamName, offset);
    return r;
  }

  // Seed + poll logs. Combined mode merges stdout and stderr client-side so
  // the build page shows phase markers and package output together (Vercel-like).
  $effect(() => {
    if (!deploy) return;
    const deployId = deploy.id;
    const streamName = stream;
    const status = deploy.status;

    lines = [];
    logMissing = false;
    logText = '';

    if (store.mode === 'preview' && store.buildLogByDeploy?.[deployId]) {
      const preview = store.buildLogByDeploy[deployId];
      lines = preview.map((l) => ({ t: l.t ?? '', kind: l.kind, text: l.text }));
      logText = preview.map((l) => l.text).join('\n');
      return;
    }

    let cancelled = false;
    let offOut = 0;
    let offErr = 0;
    let offSingle = 0;
    let startedAt = deploy.created_ms ?? Date.now();

    const stamp = () => {
      const sec = Math.max(0, Math.floor((Date.now() - startedAt) / 1000));
      return String(sec);
    };

    async function tick() {
      if (cancelled) return;
      if (streamName === 'combined') {
        const [out, err] = await Promise.all([
          pullOne(deployId, 'stdout', offOut),
          pullOne(deployId, 'stderr', offErr),
        ]);
        if (cancelled) return;
        // If both missing, surface the empty/missing state.
        if (out.missing && err.missing && !logText) {
          logMissing = true;
          return;
        }
        logMissing = false;
        const batch = [];
        if (err.lines?.length) {
          for (const l of err.lines) {
            if (!l && !l.length) continue;
            batch.push(...toTerminalLines(l, stamp()));
            logText += (logText ? '\n' : '') + l;
          }
          offErr = err.nextOffset;
        }
        if (out.lines?.length) {
          for (const l of out.lines) {
            if (!l && !l.length) continue;
            batch.push(...toTerminalLines(l, stamp()));
            logText += (logText ? '\n' : '') + l;
          }
          offOut = out.nextOffset;
        }
        if (batch.length) lines = [...lines, ...batch];
      } else {
        const r = await pullOne(deployId, streamName, offSingle);
        if (cancelled) return;
        if (r.missing) {
          logMissing = true;
          return;
        }
        logMissing = false;
        if (r.lines?.length) {
          const mapped = [];
          for (const l of r.lines) {
            mapped.push(...toTerminalLines(l, stamp()));
            logText += (logText ? '\n' : '') + l;
          }
          lines = [...lines, ...mapped];
          offSingle = r.nextOffset;
        }
      }
    }

    // Immediate seed, then poll while building (or once more after terminal
    // so the final lines land after status flips).
    tick();
    let intervalId = null;
    let timeoutId = null;
    if (status === 'building') {
      intervalId = setInterval(tick, 500);
    } else {
      // One delayed catch-up for just-finished deploys.
      timeoutId = setTimeout(tick, 400);
    }

    return () => {
      cancelled = true;
      clearInterval(intervalId);
      clearTimeout(timeoutId);
    };
  });

  const bootPhases = $derived(
    store.metrics?.boot_phases?.phases
      ? store.metrics.boot_phases.phases.map((p) => ({
          name: phaseLabel(p.name),
          ms: p.p50_ms,
          hot: false,
        }))
      : [],
  );
  const hotPhase = $derived(
    bootPhases.length ? bootPhases.reduce((a, b) => (b.ms > a.ms ? b : a)) : null,
  );
  const anatomyPhases = $derived(
    bootPhases.map((p) => ({ ...p, hot: hotPhase && p.name === hotPhase.name })),
  );
  const liveUrl = $derived(app?.domains?.[0] ? `https://${app.domains[0]}` : null);

  let rollbackOpen = $state(false);
  let rollbackTarget = $state(null);
  let rollbackError = $state('');
  let rollbackBusy = $state(false);
  const rollbackCandidates = $derived(
    app
      ? store.deploymentsFor(app.name).filter((d) => d.status === 'sealed' && d.id !== deploy?.id)
      : [],
  );
  function askRollback() {
    rollbackError = '';
    if (!rollbackCandidates.length) {
      rollbackError = 'No prior sealed deployment is available to roll back to.';
    }
    rollbackOpen = true;
    rollbackTarget = rollbackCandidates[0]?.id ?? null;
  }
  async function confirmRollback() {
    if (!app || !rollbackTarget || rollbackBusy) return;
    rollbackBusy = true;
    const target = store.deploymentsFor(app.name).find((d) => d.id === rollbackTarget);
    const expected = app.active?.artifact_hash ?? target?.artifact_hash ?? '';
    const r = await store.rollback(app.name, rollbackTarget, expected);
    rollbackBusy = false;
    if (!r.ok) {
      rollbackError = r.error ?? 'Rollback failed';
      return;
    }
    rollbackOpen = false;
  }

  function copyHash() {
    if (!deploy?.artifact_hash) return;
    navigator.clipboard?.writeText(deploy.artifact_hash).then(() => {
      copied = true;
      setTimeout(() => (copied = false), 1400);
    });
  }

  function copyLogs() {
    const text = lines.map((l) => l.text).join('\n');
    if (!text) return;
    navigator.clipboard?.writeText(text).then(() => {
      copied = true;
      setTimeout(() => (copied = false), 1400);
    });
  }
</script>

{#if deploy}
  <div class="page screen-enter">
    <header class="head">
      <div class="title">
        <div class="row1">
          <span class="pill {LED[deploy.status] === 'cold' ? 'ghost' : LED[deploy.status]}"
            >{STATUS[deploy.status] ?? deploy.status}</span
          >
          <span class="dplid num">{deploy.id}</span>
        </div>
        <h1>
          {deploy.source?.branch
            ? `${deploy.source.branch} · ${deploy.source.commit ?? shortHash(deploy.source_hash)}`
            : shortHash(deploy.source_hash)}
        </h1>
        <div class="meta num">
          {deploy.app}
          {#if deploy.source?.branch}
            · <Icon name="branch" size={11} />
            {deploy.source.branch}
          {/if}
          {#if deploy.created_ms}
            · {relativeTime(deploy.created_ms)}
          {/if}
          {#if elapsedMs != null}
            · <span class="elapsed">{formatElapsed(elapsedMs)}</span>
          {/if}
        </div>
      </div>
      <div class="actions">
        {#if deploy.status === 'active' && !app?.name?.startsWith('pr-')}
          <button class="btn" onclick={askRollback}
            ><Icon name="rollback" size={14} />Roll back</button
          >
        {/if}
        {#if liveUrl}
          <a class="btn primary" href={liveUrl} target="_blank" rel="noopener noreferrer"
            ><Icon name="ext" size={13} />Visit</a
          >
        {/if}
      </div>
    </header>

    {#if deploy.error}
      <div class="failbox"><Icon name="x" size={13} /> {deploy.error}</div>
    {/if}

    <!-- pipeline stepper -->
    <section class="card stepper">
      {#each STEPS as s, i}
        {#if i > 0}<span class="conn" class:dim={stepState[i] === 'todo'}></span>{/if}
        <div class="step {stepState[i]}">
          <span class="dot">
            {#if stepState[i] === 'done'}
              <Icon name="check" size={10} stroke={2.6} />
            {:else if stepState[i] === 'fail'}
              <Icon name="x" size={9} stroke={2.6} />
            {:else if stepState[i] === 'now'}
              <span class="pulse"></span>
            {/if}
          </span>
          <span class="sname">{s.label}</span>
        </div>
      {/each}
      <div class="grow"></div>
      <span class="total num">
        {#if deploy.status === 'building'}
          <span class="led build breathe"></span>
          Building · {formatElapsed(elapsedMs)}
        {:else if deploy.status === 'failed'}
          <span class="led fail"></span>
          Failed{#if elapsedMs != null}
            · {formatElapsed(elapsedMs)}
          {/if}
        {:else if deploy.status === 'active'}
          <span class="led live"></span>
          Live{#if elapsedMs != null}
            · {formatElapsed(elapsedMs)}
          {/if}
        {:else}
          Sealed · pending activation
        {/if}
      </span>
    </section>

    <div class="grid">
      <section class="card logcard">
        <div class="cardhead">
          <span class="label">Build output</span>
          <div class="logtools">
            <div class="seg mini">
              <button class:on={stream === 'combined'} onclick={() => (stream = 'combined')}
                >All</button
              >
              <button class:on={stream === 'stdout'} onclick={() => (stream = 'stdout')}
                >stdout</button
              >
              <button class:on={stream === 'stderr'} onclick={() => (stream = 'stderr')}
                >stderr</button
              >
            </div>
            <button class="btn icon sm" onclick={copyLogs} title="Copy log" aria-label="Copy log">
              <Icon name="copy" size={13} />
            </button>
          </div>
        </div>
        <div class="termwrap">
          {#if lines.length}
            <Terminal {lines} building={deploy.status === 'building'} follow={true} maxHeight="560px" />
            {#if logMissing}
              <div class="lognote mono">
                could not refresh logs from the daemon · showing buffered lines
              </div>
            {/if}
          {:else if deploy.error}
            <div class="term-empty mono fail">
              {deploy.error}
              {#if logMissing}
                <div class="lognote">
                  build log files were not readable via the API — the error above is from the
                  deployment record
                </div>
              {/if}
            </div>
          {:else if logMissing}
            <div class="term-empty mono">
              {deploy.status === 'building'
                ? 'Provisioning build… log stream will appear in a moment'
                : 'build log not registered for this deployment'}
            </div>
          {:else}
            <div class="term-empty mono">
              {#if deploy.status === 'building'}
                <span class="waitdot"></span> Collecting build output…
              {:else}
                no log output captured
              {/if}
            </div>
          {/if}
        </div>
      </section>

      <aside class="side">
        <section class="card">
          <div class="cardhead"><span class="label">Deployment</span></div>
          <div class="kv">
            <div class="kvrow"><span>Status</span><b class="num">{STATUS[deploy.status] ?? deploy.status}</b></div>
            <div class="kvrow"><span>Engine</span><b class="num">{deploy.engine_version ?? '—'}</b></div>
            <div class="kvrow"
              ><span>Source</span><b class="num">{shortHash(deploy.source_hash)}</b></div
            >
            <div class="kvrow">
              <span>Artifact</span>
              <button
                class="hashbtn num"
                onclick={copyHash}
                title={deploy.artifact_hash ?? ''}
              >
                {shortHash(deploy.artifact_hash)}
                {#if copied}<span class="copied">copied</span>{/if}
              </button>
            </div>
            {#if liveUrl}
              <div class="kvrow">
                <span>URL</span>
                <a class="num liveurl" href={liveUrl} target="_blank" rel="noopener noreferrer"
                  >{app.domains[0]}</a
                >
              </div>
            {/if}
            {#if elapsedMs != null}
              <div class="kvrow"><span>Duration</span><b class="num">{formatElapsed(elapsedMs)}</b></div>
            {/if}
          </div>
          <div class="foot num">content-addressed · RO-mounted · runtime writes are noexec</div>
        </section>

        <section class="card">
          <div class="cardhead">
            <span class="label">Revival anatomy</span>
            {#if store.metrics?.boot_phases}
              <span class="p num">p50 {millis(store.metrics.totals.boot_p50_ms)}</span>
            {/if}
          </div>
          {#if anatomyPhases.length}
            <div class="anat"><Anatomy phases={anatomyPhases} /></div>
          {:else}
            <div class="anat-empty mono">no boots sampled yet</div>
          {/if}
        </section>

        {#if rollbackCandidates.length}
          <section class="card bluegreen">
            <div class="bgrow">
              <Icon name="rollback" size={15} />
              <p>
                {rollbackCandidates.length} prior artifact{rollbackCandidates.length === 1
                  ? ''
                  : 's'} retained. Rollback is the same swap pointed backwards — instant, no rebuild.
              </p>
            </div>
          </section>
        {/if}
      </aside>
    </div>
  </div>

  {#if rollbackOpen}
    <div
      class="scrim"
      onclick={(e) => {
        if (e.target === e.currentTarget) rollbackOpen = false;
      }}
      role="presentation"
    >
      <div class="dialog" role="dialog" aria-label="Confirm rollback">
        <h2>Roll back {app?.name ?? 'app'}?</h2>
        <p class="dlg-copy">
          Instantly activate a prior sealed artifact. No rebuild. Traffic moves on the next request.
        </p>
        {#if rollbackCandidates.length}
          <label class="field">
            <span>Target deployment</span>
            <select bind:value={rollbackTarget}>
              {#each rollbackCandidates as c}
                <option value={c.id}>{c.id} · {shortHash(c.artifact_hash)}</option>
              {/each}
            </select>
          </label>
        {/if}
        {#if rollbackError}
          <div class="failbox tight">{rollbackError}</div>
        {/if}
        <div class="dlg-actions">
          <button class="btn" onclick={() => (rollbackOpen = false)}>Cancel</button>
          <button
            class="btn danger"
            disabled={!rollbackTarget || rollbackBusy || !rollbackCandidates.length}
            onclick={confirmRollback}
          >
            {rollbackBusy ? 'Rolling back…' : 'Confirm rollback'}
          </button>
        </div>
      </div>
    </div>
  {/if}
{:else}
  <div class="page screen-enter">
    <div class="empty mono">no deployment selected</div>
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
    align-items: flex-end;
    justify-content: space-between;
    gap: 24px;
    margin-bottom: 18px;
  }
  .row1 {
    display: flex;
    align-items: center;
    gap: 11px;
    margin-bottom: 10px;
  }
  .dplid {
    font-size: 12px;
    color: var(--ink-3);
  }
  h1 {
    font-size: 21px;
    font-weight: 650;
    letter-spacing: -0.018em;
    max-width: 640px;
  }
  .meta {
    display: flex;
    align-items: center;
    gap: 6px;
    margin-top: 8px;
    font-size: 11.5px;
    color: var(--ink-3);
  }
  .elapsed {
    color: var(--ink-2);
    font-variant-numeric: tabular-nums;
  }
  .actions {
    display: flex;
    gap: 9px;
    flex: none;
  }
  .btn.primary {
    background: var(--ink);
    color: #fff;
    border-color: var(--ink);
  }
  .btn.primary:hover {
    filter: brightness(1.08);
  }

  .stepper {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 16px 20px;
    margin-bottom: 18px;
    flex-wrap: wrap;
  }
  .step {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .dot {
    width: 19px;
    height: 19px;
    border-radius: 50%;
    background: var(--ink);
    color: #fff;
    display: grid;
    place-items: center;
    flex: none;
  }
  .step.todo .dot {
    background: transparent;
    box-shadow: inset 0 0 0 1.5px var(--ink-4);
  }
  .step.now .dot {
    background: var(--amber);
    animation: led-breathe 1.6s ease-in-out infinite;
  }
  .step.fail .dot {
    background: var(--red);
  }
  .pulse {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #fff;
    opacity: 0.95;
  }
  .sname {
    font-size: 12.5px;
    font-weight: 600;
  }
  .step.todo .sname {
    color: var(--ink-4);
    font-weight: 500;
  }
  .conn {
    width: 26px;
    height: 1.5px;
    background: var(--line-strong);
    border-radius: 1px;
  }
  .conn.dim {
    background: var(--line-2);
  }
  .grow {
    flex: 1;
  }
  .total {
    font-size: 11.5px;
    color: var(--ink-2);
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-variant-numeric: tabular-nums;
  }

  .grid {
    display: grid;
    grid-template-columns: 1fr 322px;
    gap: 20px;
    align-items: start;
  }
  .side {
    display: flex;
    flex-direction: column;
    gap: 16px;
  }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
    gap: 12px;
  }
  .logtools {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .termwrap {
    padding: 0 12px 12px;
  }
  .lognote {
    margin-top: 8px;
    font-size: 11px;
    color: var(--ink-4);
  }
  .term-empty {
    min-height: 220px;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    justify-content: center;
    gap: 8px;
    padding: 28px 18px;
    color: var(--ink-3);
    font-size: 12.5px;
  }
  .term-empty.fail {
    color: var(--red);
  }
  .waitdot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--amber);
    display: inline-block;
    margin-right: 8px;
    animation: led-breathe 1.4s ease-in-out infinite;
  }

  .kv {
    padding: 2px 10px 4px;
  }
  .kvrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8.5px 8px;
    font-size: 12.5px;
  }
  .kvrow + .kvrow {
    border-top: 1px solid var(--line-2);
  }
  .kvrow span {
    color: var(--ink-3);
  }
  .kvrow b {
    color: var(--ink);
    font-weight: 500;
    font-size: 12px;
  }
  .hashbtn {
    background: none;
    border: 0;
    padding: 0;
    cursor: pointer;
    color: var(--ink);
    font: inherit;
  }
  .hashbtn:hover {
    color: var(--cobalt-deep);
  }
  .copied {
    margin-left: 6px;
    color: var(--cobalt-deep);
    font-size: 10.5px;
  }
  .liveurl {
    color: var(--cobalt-deep);
    text-decoration: none;
  }
  .liveurl:hover {
    text-decoration: underline;
  }
  .foot {
    padding: 10px 18px 14px;
    font-size: 10.5px;
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
    margin-top: 6px;
  }
  .p {
    font-size: 11px;
    color: var(--cobalt-deep);
  }
  .anat {
    padding: 4px 12px 14px;
  }
  .anat-empty {
    padding: 18px;
    color: var(--ink-4);
    font-size: 12px;
  }

  .bluegreen {
    padding: 14px 16px;
  }
  .bgrow {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    color: var(--ink-2);
    font-size: 12.5px;
    line-height: 1.45;
  }
  .bgrow p {
    margin: 0;
  }

  .failbox {
    display: flex;
    align-items: flex-start;
    gap: 8px;
    padding: 12px 14px;
    margin-bottom: 14px;
    border-radius: var(--r-m);
    background: var(--red-soft);
    color: var(--red);
    font-size: 13px;
    line-height: 1.4;
  }
  .failbox.tight {
    margin: 10px 0 0;
  }

  .scrim {
    position: fixed;
    inset: 0;
    background: rgba(8, 10, 14, 0.45);
    display: grid;
    place-items: center;
    z-index: 40;
    padding: 24px;
  }
  .dialog {
    width: min(440px, 100%);
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: var(--r-l);
    padding: 22px 22px 18px;
    box-shadow: 0 18px 50px rgba(0, 0, 0, 0.18);
  }
  .dialog h2 {
    margin: 0 0 8px;
    font-size: 17px;
    font-weight: 650;
  }
  .dlg-copy {
    margin: 0 0 14px;
    color: var(--ink-3);
    font-size: 13px;
    line-height: 1.45;
  }
  .field {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
    color: var(--ink-3);
  }
  .field select {
    font: inherit;
    padding: 8px 10px;
    border-radius: var(--r-s);
    border: 1px solid var(--line);
    background: var(--surface-2);
    color: var(--ink);
  }
  .dlg-actions {
    display: flex;
    justify-content: flex-end;
    gap: 8px;
    margin-top: 16px;
  }
  .btn.danger {
    color: var(--red);
  }
  .btn.danger:hover {
    background: var(--red-soft);
  }

  .empty {
    padding: 80px 0;
    text-align: center;
    color: var(--ink-4);
  }

  @media (max-width: 980px) {
    .grid {
      grid-template-columns: 1fr;
    }
    .page {
      padding: 20px 18px 0;
    }
  }
</style>
