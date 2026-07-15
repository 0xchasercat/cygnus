<script>
  import { ui } from '../stores.svelte.js';
  import { apps, deploys, buildLog, node } from '../data.js';
  import Icon from '../components/Icon.svelte';
  import Terminal from '../components/Terminal.svelte';
  import Anatomy from '../components/Anatomy.svelte';

  const app = $derived(apps.find((a) => a.id === ui.appId) ?? apps[0]);
  const deploy = $derived(
    deploys.find((d) => d.id === ui.deployId) ?? deploys.find((d) => d.app === app.id) ?? deploys[0]
  );

  const LED = { live: 'live', building: 'build', failed: 'fail', preview: 'preview', previous: 'cold' };
  const STATUS = { live: 'production', building: 'building', failed: 'failed', preview: 'preview', previous: 'retained' };

  const steps = [
    { name: 'Queued', t: '0.2s' },
    { name: 'Build cage', t: '0.7s' },
    { name: 'Install', t: '3.8s' },
    { name: 'Bundle + bytecode', t: '2.9s' },
    { name: 'Blue-green swap', t: '0.9s' },
    { name: 'Live', t: '' },
  ];

  // step states depend on deploy status
  const stepState = $derived.by(() => {
    if (deploy.status === 'building') return steps.map((_, i) => (i < 3 ? 'done' : i === 3 ? 'now' : 'todo'));
    if (deploy.status === 'failed') return steps.map((_, i) => (i < 3 ? 'done' : i === 3 ? 'fail' : 'todo'));
    return steps.map(() => 'done');
  });

  const logLines = $derived(
    deploy.status === 'building' ? buildLog.slice(0, 9) : deploy.status === 'failed' ? [...buildLog.slice(0, 8), { t: '6.31', kind: 'err', text: 'build failed · module "edge-kv" resolves no entrypoint for target bun' }] : buildLog
  );
</script>

<div class="page screen-enter">
  <header class="head">
    <div class="title">
      <div class="row1">
        <span class="pill {LED[deploy.status] === 'cold' ? 'ghost' : LED[deploy.status]}">{STATUS[deploy.status]}</span>
        <span class="dplid num">{deploy.id}</span>
      </div>
      <h1>{deploy.commit}</h1>
      <div class="meta num">
        {app.name} · <Icon name="branch" size={11} /> {deploy.branch} · {deploy.author} · {deploy.when}
      </div>
    </div>
    <div class="actions">
      {#if deploy.status === 'live'}
        <button class="btn" disabled title="Unavailable: daemon admin bridge offline"><Icon name="rollback" size={14} />Roll back</button>
      {:else if deploy.status !== 'failed' && deploy.status !== 'building'}
        <button class="btn cobalt" disabled title="Unavailable: daemon admin bridge offline"><Icon name="ship" size={13} />Promote to production</button>
      {/if}
      <button class="btn"><Icon name="ext" size={13} />Open</button>
    </div>
  </header>

  <!-- ————— pipeline stepper ————— -->
  <section class="card stepper">
    {#each steps as s, i}
      {#if i > 0}<span class="conn" class:dim={stepState[i] === 'todo'}></span>{/if}
      <div class="step {stepState[i]}">
        <span class="dot">
          {#if stepState[i] === 'done'}
            <Icon name="check" size={10} stroke={2.6} />
          {:else if stepState[i] === 'fail'}
            <Icon name="x" size={9} stroke={2.6} />
          {/if}
        </span>
        <span class="sname">{s.name}</span>
        {#if s.t && stepState[i] !== 'todo'}<span class="stime num">{s.t}</span>{/if}
      </div>
    {/each}
    <div class="grow"></div>
    <span class="total num">
      {#if deploy.status === 'building'}
        <span class="led build breathe"></span> running · 6.2s
      {:else if deploy.status === 'failed'}
        failed at 6.3s
      {:else}
        source → live in {deploy.dur}
      {/if}
    </span>
  </section>

  <div class="grid">
    <section class="card logcard">
      <div class="cardhead">
        <span class="label">Build log · server-side</span>
        <span class="cagehint num">build cage · egress allowlisted · lifecycle scripts off</span>
      </div>
      <div class="termwrap">
        <Terminal lines={logLines} building={deploy.status === 'building'} />
      </div>
    </section>

    <aside class="side">
      <section class="card">
        <div class="cardhead"><span class="label">Artifact</span></div>
        <div class="kv">
          <div class="kvrow"><span>bundle.js</span><b class="num">1.24 MB</b></div>
          <div class="kvrow"><span>bundle.jsc</span><b class="num">3.41 MB · bytecode</b></div>
          <div class="kvrow"><span>Engine</span><b class="num">bun 1.2.19 · pinned</b></div>
          <div class="kvrow"><span>Content hash</span><b class="num">ab9e02f1</b></div>
        </div>
        <div class="foot num">content-addressed · RO-mounted · runtime writes are noexec</div>
      </section>

      <section class="card">
        <div class="cardhead">
          <span class="label">Revival anatomy</span>
          <span class="p num">p50 {node.coldStart.p50} ms</span>
        </div>
        <div class="anat">
          <Anatomy phases={node.coldStart.phases} />
        </div>
      </section>

      <section class="card bluegreen">
        <div class="bgrow">
          <Icon name="rollback" size={15} />
          <p>Five artifacts retained. Rollback is the same swap pointed backwards — instant, no rebuild.</p>
        </div>
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
</style>
