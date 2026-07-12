<script>
  import { node } from '../data.js';
  import Anatomy from '../components/Anatomy.svelte';
  import Constellation from '../components/Constellation.svelte';
  import Icon from '../components/Icon.svelte';

  const pct = (gb) => (gb / node.ram) * 100;
  const free = $derived(node.ram - node.ramCages - node.ramEngines - node.ramSystem);
  const maxEgress = $derived(Math.max(...node.egress.top.map((t) => t.gb)));
</script>

<div class="page screen-enter">
  <header class="head">
    <div>
      <div class="row1">
        <h1>{node.name}</h1>
        <span class="led live breathe"></span>
      </div>
      <p class="sub num">{node.binary}</p>
      <div class="hostchips">
        <span class="chip">kernel <b>{node.kernel}</b></span>
        <span class="chip"><span class="led live"></span> patched {node.kernelPatched}</span>
        <span class="chip">uptime <b>{node.uptime}</b></span>
        <span class="chip">{node.ram} GB · 8 vCPU</span>
      </div>
    </div>
    <div class="constellation">
      <Constellation w={250} opacity={0.85} />
    </div>
  </header>

  <div class="grid">
    <!-- ————— density ————— -->
    <section class="card">
      <div class="cardhead">
        <span class="label">Memory · density</span>
        <span class="hint num">{node.ramUsed} / {node.ram} GB</span>
      </div>
      <div class="pad">
        <div class="rambar">
          <i style="width:{pct(node.ramCages)}%" class="b-cages"></i>
          <i style="width:{pct(node.ramEngines)}%" class="b-engines"></i>
          <i style="width:{pct(node.ramSystem)}%" class="b-system"></i>
        </div>
        <div class="ramlegend">
          <span><i class="dot b-cages"></i>warm cages <b class="num">{node.ramCages} GB</b></span>
          <span><i class="dot b-engines"></i>engine text <b class="num">{Math.round(node.ramEngines * 1000)} MB</b></span>
          <span><i class="dot b-system"></i>system <b class="num">{node.ramSystem} GB</b></span>
          <span><i class="dot b-free"></i>free <b class="num">{free.toFixed(1)} GB</b></span>
        </div>
        <div class="counts">
          <div class="count">
            <span class="readout md">{node.warm}</span>
            <span class="label">warm</span>
          </div>
          <div class="hairline-v"></div>
          <div class="count">
            <span class="readout md">{node.registered}</span>
            <span class="label">registered</span>
          </div>
          <div class="hairline-v"></div>
          <div class="count">
            <span class="readout md">{node.registered - node.warm}</span>
            <span class="label">asleep · disk only</span>
          </div>
        </div>
        <p class="axiom">Density is bounded by concurrent-active apps, not registered apps.</p>
      </div>
    </section>

    <!-- ————— cold starts ————— -->
    <section class="card">
      <div class="cardhead">
        <span class="label">Revival budget</span>
        <span class="hint num">p50 <b>{node.coldStart.p50} ms</b> · p99 <b>{node.coldStart.p99} ms</b> · target ≤ 150</span>
      </div>
      <div class="pad">
        <Anatomy phases={node.coldStart.phases} />
      </div>
    </section>

    <!-- ————— engines ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Engines · page-cache shared</span></div>
      <div class="rows pad0">
        {#each node.engines as e}
          <div class="engine">
            <span class="ename num">{e.version}</span>
            {#if e.def}<span class="pill cobalt">default</span>{/if}
            <span class="grow"></span>
            <span class="emeta num">{e.text} text · {e.apps} apps</span>
          </div>
        {/each}
      </div>
      <div class="foot num">one text copy per resident version · unreferenced engines are GC’d</div>
    </section>

    <!-- ————— certificates ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Certificates · ACME</span></div>
      <div class="rows pad0">
        {#each node.certs as c}
          <div class="cert">
            <span class="led live"></span>
            <span class="cdomain num">{c.domain}</span>
            <span class="ckind num">{c.kind}</span>
            <span class="grow"></span>
            <span class="crenew num">renews {c.renews}</span>
          </div>
        {/each}
      </div>
      <div class="foot num">keys never enter a cage · hot-loaded into rustls</div>
    </section>

    <!-- ————— egress ————— -->
    <section class="card">
      <div class="cardhead">
        <span class="label">Egress · nftables per cage</span>
        <span class="hint num">{node.egress.today} today · {node.egress.conns} conns</span>
      </div>
      <div class="pad">
        <div class="modes">
          <span class="chip">public <b>{node.egress.modes.public}</b></span>
          <span class="chip">restricted <b>{node.egress.modes.restricted}</b></span>
          <span class="chip">none <b>{node.egress.modes.none}</b></span>
          <span class="chip">open <b>{node.egress.modes.open}</b></span>
        </div>
        <div class="topapps">
          {#each node.egress.top as t}
            <div class="tapp">
              <span class="tname num">{t.app}</span>
              <span class="tbar"><i style="width:{(t.gb / maxEgress) * 100}%"></i></span>
              <span class="tgb num">{t.gb} GB</span>
            </div>
          {/each}
        </div>
      </div>
    </section>

    <!-- ————— break-glass ————— -->
    <section class="card">
      <div class="cardhead"><span class="label">Break-glass</span></div>
      <div class="pad">
        <p class="bgtext">
          If Tenant 0 — this dashboard — ever bricks itself, the node doesn’t care.
          <code>cygnusctl</code> talks to the daemon over a root-only socket, past everything.
        </p>
        <div class="code num">
          <span class="p">$</span> cygnusctl status --socket /run/cygnus.sock
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
