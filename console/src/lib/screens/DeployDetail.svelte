<script>
  import { ui, openDeploy } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { shortHash, millis, phaseLabel } from '../fmt.js';
  import Icon from '../components/Icon.svelte';
  import Terminal from '../components/Terminal.svelte';
  import Anatomy from '../components/Anatomy.svelte';

  const app = $derived(store.appByName(ui.appId) ?? (store.deploymentById(ui.deployId)?.app ? store.appByName(store.deploymentById(ui.deployId).app) : null) ?? null);
  const deploy = $derived(
    store.deploymentById(ui.deployId) ??
      (app ? store.deploymentsFor(app.name)[0] : null) ??
      store.deployments[0] ??
      null
  );

  const LED = { active: 'live', building: 'build', failed: 'fail', sealed: 'cold' };
  const STATUS = { active: 'live', building: 'building', failed: 'failed', sealed: 'sealed' };

  const STEPS = ['Source', 'Build', 'Seal', 'Activate'];

  const stepState = $derived.by(() => {
    if (!deploy) return ['todo', 'todo', 'todo', 'todo'];
    if (deploy.status === 'building') return ['done', 'now', 'todo', 'todo'];
    if (deploy.status === 'failed') {
      // failure surfaces during build/seal — show red on step 2.
      return ['done', 'fail', 'todo', 'todo'];
    }
    if (deploy.status === 'sealed') return ['done', 'done', 'done', 'todo'];
    if (deploy.status === 'active') return ['done', 'done', 'done', 'done'];
    return ['todo', 'todo', 'todo', 'todo'];
  });

  // ——— live build logs ———
  let stream = $state('stdout');
  let lines = $state([]);
  let logMissing = $state(false);
  let logTimer = $state(null);
  let copied = $state(false);
  // offset is intentionally NOT $state — it advances inside the polling loop
  // and must not retrigger the seeding effect (which would wipe lines).
  let logOffset = 0;

  const previewLog = $derived(deploy ? store.buildLogByDeploy?.[deploy.id] : null);

  function classifyLine(text) {
    if (/^\$/i.test(text)) return 'head';
    if (/error|failed/i.test(text)) return 'err';
    if (/warn/i.test(text)) return 'dim';
    return 'text';
  }

  function toTerminalLines(text) {
    const raw = text.length ? text.split('\n').filter((l) => l.length) : [];
    return raw.map((t) => ({ t: '', kind: classifyLine(t), text: t }));
  }

  async function pullLogs(deployId, streamName, offset) {
    const r = await store.fetchDeploymentLog(deployId, streamName, offset);
    if (r.missing) {
      logMissing = true;
      return offset;
    }
    logMissing = false;
    if (r.lines.length) {
      const mapped = r.lines.flatMap((l) => toTerminalLines(l));
      lines = [...lines, ...mapped];
      return r.nextOffset;
    }
    return offset;
  }

  // Seed preview fixture logs, or start live polling when the deploy is opened.
  // Re-runs only when the viewed deploy id or stream changes (offset is untracked).
  $effect(() => {
    if (!deploy) return;
    const deployId = deploy.id;
    const streamName = stream;
    const status = deploy.status;

    lines = [];
    logMissing = false;
    logOffset = 0;

    if (store.mode === 'preview' && previewLog) {
      lines = previewLog.map((l) => ({ t: '', kind: l.kind, text: l.text }));
      return;
    }

    let offset = 0;
    let timer = null;
    // seed once
    pullLogs(deployId, streamName, offset).then((next) => { offset = next; logOffset = next; });
    if (status === 'building') {
      timer = setInterval(async () => {
        offset = await pullLogs(deployId, streamName, offset);
        logOffset = offset;
      }, 1000);
      logTimer = timer;
    }
    return () => {
      if (timer) clearInterval(timer);
      logTimer = null;
    };
  });

  const bootPhases = $derived(
    store.metrics?.boot_phases?.phases
      ? store.metrics.boot_phases.phases.map((p) => ({
          name: phaseLabel(p.name),
          ms: p.p50_ms,
          hot: false,
        }))
      : []
  );
  const hotPhase = $derived(
    bootPhases.length ? bootPhases.reduce((a, b) => (b.ms > a.ms ? b : a)) : null
  );
  const anatomyPhases = $derived(
    bootPhases.map((p) => ({ ...p, hot: hotPhase && p.name === hotPhase.name }))
  );
  const liveUrl = $derived(app?.domains?.[0] ? `https://${app.domains[0]}` : null);

  // rollback (same flow as AppDetail)
  let rollbackOpen = $state(false);
  let rollbackTarget = $state(null);
  let rollbackError = $state('');
  let rollbackBusy = $state(false);
  const rollbackCandidates = $derived(
    app ? store.deploymentsFor(app.name).filter((d) => d.status === 'sealed' && d.id !== deploy?.id) : []
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
    if (!r.ok) { rollbackError = r.error ?? 'Rollback failed'; return; }
    rollbackOpen = false;
  }

  function copyHash() {
    if (!deploy?.artifact_hash) return;
    navigator.clipboard?.writeText(deploy.artifact_hash).then(() => {
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
          <span class="pill {LED[deploy.status] === 'cold' ? 'ghost' : LED[deploy.status]}">{STATUS[deploy.status] ?? deploy.status}</span>
          <span class="dplid num">{deploy.id}</span>
        </div>
        <h1>{deploy.source?.branch ? `${deploy.source.branch} · ${deploy.source.commit ?? shortHash(deploy.source_hash)}` : shortHash(deploy.source_hash)}</h1>
        <div class="meta num">
          {deploy.app}
          {#if deploy.source?.branch} · <Icon name="branch" size={11} /> {deploy.source.branch}{/if}
          {#if deploy.created_ms} · {relativeTime(deploy.created_ms)}{/if}
        </div>
      </div>
      <div class="actions">
        {#if deploy.status === 'active' && !app?.name.startsWith('pr-')}
          <button class="btn" onclick={askRollback}><Icon name="rollback" size={14} />Roll back</button>
        {/if}
        {#if liveUrl}
          <a class="btn" href={liveUrl} target="_blank" rel="noopener noreferrer"><Icon name="ext" size={13} />Open</a>
        {/if}
      </div>
    </header>

    {#if deploy.error}
      <div class="failbox"><Icon name="x" size={13} /> {deploy.error}</div>
    {/if}

    <!-- ————— pipeline stepper ————— -->
    <section class="card stepper">
      {#each STEPS as s, i}
        {#if i > 0}<span class="conn" class:dim={stepState[i] === 'todo'}></span>{/if}
        <div class="step {stepState[i]}">
          <span class="dot">
            {#if stepState[i] === 'done'}
              <Icon name="check" size={10} stroke={2.6} />
            {:else if stepState[i] === 'fail'}
              <Icon name="x" size={9} stroke={2.6} />
            {/if}
          </span>
          <span class="sname">{s}</span>
        </div>
      {/each}
      <div class="grow"></div>
      <span class="total num">
        {#if deploy.status === 'building'}
          <span class="led build breathe"></span> running
        {:else if deploy.status === 'failed'}
          failed
        {:else if deploy.status === 'active'}
          <span class="led live"></span> live
        {:else}
          sealed · pending activation
        {/if}
      </span>
    </section>

    <div class="grid">
      <section class="card logcard">
        <div class="cardhead">
          <span class="label">Build log · server-side</span>
          <div class="seg mini">
            <button class:on={stream === 'stdout'} onclick={() => (stream = 'stdout')}>stdout</button>
            <button class:on={stream === 'stderr'} onclick={() => (stream = 'stderr')}>stderr</button>
          </div>
        </div>
        <div class="termwrap">
          {#if lines.length}
            <Terminal {lines} building={deploy.status === 'building'} follow={true} />
            {#if logMissing}
              <div class="lognote mono">could not refresh logs from the daemon (404) · showing buffered lines</div>
            {/if}
          {:else if deploy.error}
            <div class="term-empty mono fail">
              {deploy.error}
              {#if logMissing}
                <div class="lognote">build log files were not readable via the API — the error above is from the deployment record</div>
              {/if}
            </div>
          {:else if logMissing}
            <div class="term-empty mono">build log not registered for this deployment yet</div>
          {:else}
            <div class="term-empty mono">{deploy.status === 'building' ? 'collecting build output…' : 'no log output captured'}</div>
          {/if}
        </div>
      </section>

      <aside class="side">
        <section class="card">
          <div class="cardhead"><span class="label">Artifact</span></div>
          <div class="kv">
            <div class="kvrow"><span>Engine</span><b class="num">{deploy.engine_version ?? '—'}</b></div>
            <div class="kvrow"><span>Source hash</span><b class="num">{shortHash(deploy.source_hash)}</b></div>
            <div class="kvrow">
              <span>Artifact</span>
              <button class="hashbtn num" onclick={copyHash} title={deploy.artifact_hash ?? ''}>
                {shortHash(deploy.artifact_hash)}
                {#if copied}<span class="copied">copied</span>{/if}
              </button>
            </div>
            {#if liveUrl}
              <div class="kvrow"><span>Live</span><a class="num liveurl" href={liveUrl} target="_blank" rel="noopener noreferrer">{app.domains[0]}</a></div>
            {/if}
          </div>
          <div class="foot num">content-addressed · RO-mounted · runtime writes are noexec</div>
        </section>

        <section class="card">
          <div class="cardhead">
            <span class="label">Revival anatomy</span>
            {#if store.metrics?.boot_phases}<span class="p num">p50 {millis(store.metrics.totals.boot_p50_ms)}</span>{/if}
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
              <p>{rollbackCandidates.length} prior artifact{rollbackCandidates.length === 1 ? '' : 's'} retained. Rollback is the same swap pointed backwards — instant, no rebuild.</p>
            </div>
          </section>
        {/if}
      </aside>
    </div>
  </div>

  {#if rollbackOpen}
    <div class="scrim" onclick={(e) => { if (e.target === e.currentTarget) rollbackOpen = false; }} role="presentation">
      <div class="dialog" role="dialog" aria-label="Confirm rollback">
        <p class="eyebrow">CONFIRM ROLLBACK</p>
        <h2>Swap {deploy.app} to a prior deployment?</h2>
        <p class="dcopy">The active artifact is checked (CAS) before the retained deployment is promoted. No rebuild is started.</p>
        {#if rollbackCandidates.length}
          <div class="targets">
            {#each rollbackCandidates as d (d.id)}
              <label class="target">
                <input type="radio" name="rb2" value={d.id} bind:group={rollbackTarget} />
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
  .dplid { font-size: 12px; color: var(--ink-3); }
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
  .actions { display: flex; gap: 9px; flex: none; }

  /* stepper */
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
  .step.fail .dot { background: var(--red); }
  .sname { font-size: 12.5px; font-weight: 600; }
  .step.todo .sname { color: var(--ink-4); font-weight: 500; }
  .stime { font-size: 10.5px; color: var(--ink-3); }
  .conn {
    width: 26px;
    height: 1.5px;
    background: var(--line-strong);
    border-radius: 1px;
  }
  .conn.dim { background: var(--line-2); }
  .grow { flex: 1; }
  .total {
    font-size: 11.5px;
    color: var(--ink-2);
    display: inline-flex;
    align-items: center;
    gap: 8px;
  }

  .grid {
    display: grid;
    grid-template-columns: 1fr 322px;
    gap: 20px;
    align-items: start;
  }
  .side { display: flex; flex-direction: column; gap: 16px; }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
    gap: 12px;
  }
  .cagehint { font-size: 10.5px; color: var(--ink-4); }
  .termwrap { padding: 0 12px 12px; }

  .kv { padding: 2px 10px 4px; }
  .kvrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 8.5px 8px;
    font-size: 12.5px;
  }
  .kvrow + .kvrow { border-top: 1px solid var(--line-2); }
  .kvrow span { color: var(--ink-3); }
  .kvrow b { color: var(--ink); font-weight: 500; font-size: 12px; }
  .foot {
    padding: 10px 18px 14px;
    font-size: 10.5px;
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
    margin-top: 6px;
  }
  .p { font-size: 11px; color: var(--cobalt-deep); }
  .anat { padding: 6px 18px 18px; }

  .bluegreen { padding: 15px 17px; }
  .bgrow {
    display: flex;
    gap: 12px;
    align-items: flex-start;
    color: var(--ink-3);
  }
  .bgrow p {
    font-size: 12px;
    line-height: 1.6;
    color: var(--ink-2);
  }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
  }

  .failbox {
    display: flex;
    align-items: center;
    gap: 9px;
    margin-bottom: 16px;
    padding: 12px 16px;
    border: 1px solid color-mix(in srgb, var(--red) 35%, var(--line));
    border-radius: 12px;
    background: var(--red-soft);
    color: var(--red);
    font-size: 12.5px;
    line-height: 1.5;
  }
  .seg.mini button { height: 22px; padding: 0 10px; font-size: 11px; }
  .lognote {
    margin-top: 8px;
    font-size: 10px;
    color: var(--ink-4);
    letter-spacing: 0.04em;
  }
  .term-empty {
    padding: 40px 18px;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.04em;
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: var(--r-m);
    white-space: pre-wrap;
    word-break: break-word;
  }
  .term-empty.fail {
    color: var(--ink-2);
    border-color: color-mix(in srgb, var(--red) 30%, var(--line-2));
    background: color-mix(in srgb, var(--red) 8%, var(--surface-3));
  }
  .term-empty .lognote {
    margin-top: 10px;
  }
  .hashbtn {
    font-family: var(--mono);
    font-size: 12px;
    color: var(--ink);
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: 7px;
    padding: 3px 9px;
    cursor: pointer;
    display: inline-flex;
    align-items: center;
    gap: 7px;
  }
  .hashbtn:hover { border-color: var(--cobalt); color: var(--cobalt-deep); }
  .copied { font-size: 9.5px; color: #087a45; letter-spacing: 0.06em; }
  .liveurl { color: var(--cobalt-deep); }
  .liveurl:hover { text-decoration: underline; }
  .anat-empty {
    padding: 28px 18px;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }
  .empty {
    padding: 60px 0;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }

  /* rollback dialog (shared shape with AppDetail) */
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
  .eyebrow { color: var(--amber); font: 600 10px var(--mono); letter-spacing: 0.15em; text-transform: uppercase; }
  .dialog h2 { margin-top: 8px; font-size: 17px; letter-spacing: -0.015em; }
  .dcopy { margin-top: 8px; font-size: 12.5px; color: var(--ink-3); line-height: 1.55; }
  .targets { margin-top: 16px; display: flex; flex-direction: column; gap: 6px; max-height: 200px; overflow-y: auto; }
  .target {
    display: flex; align-items: center; gap: 10px;
    padding: 10px 12px; border: 1px solid var(--line-2); border-radius: 10px;
    cursor: pointer; font-size: 12px;
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
