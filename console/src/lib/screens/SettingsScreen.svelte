<script>
  import { team, tokens, node } from '../data.js';
  import Icon from '../components/Icon.svelte';
</script>

<div class="page screen-enter">
  <div class="head">
    <h1>Settings</h1>
    <p class="sub">One node, one binary. Everything else is a row in SQLite.</p>
  </div>

  <div class="grid">
    <div class="col">
      <!-- team -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Team</span>
          <button class="btn sm" disabled title="Unavailable: daemon admin bridge offline"><Icon name="plus" size={12} />Invite</button>
        </div>
        <div class="rows pad0">
          {#each team as m}
            <div class="row">
              <span class="mavatar" style="background:{m.color}">{m.name[0].toUpperCase()}</span>
              <span class="mname">{m.name}</span>
              <span class="grow"></span>
              <span class="pill ghost">{m.role}</span>
            </div>
          {/each}
        </div>
        <div class="foot num">every action attributed · append-only audit log</div>
      </section>

      <!-- tokens -->
      <section class="card">
        <div class="cardhead">
          <span class="label">API tokens</span>
          <button class="btn sm" disabled title="Unavailable: daemon admin bridge offline"><Icon name="key" size={12} />New token</button>
        </div>
        <div class="rows pad0">
          {#each tokens as t}
            <div class="row">
              <span class="glyph"><Icon name="key" size={13} /></span>
              <div class="tk">
                <span class="mname">{t.name}</span>
                <span class="tmeta num">{t.prefix} · {t.scope} · used {t.last}</span>
              </div>
              <span class="grow"></span>
              <button class="btn sm danger" disabled title="Unavailable: daemon admin bridge offline">Revoke</button>
            </div>
          {/each}
        </div>
      </section>

      <!-- gitops -->
      <section class="card">
        <div class="cardhead"><span class="label">GitOps</span></div>
        <div class="rows pad0">
          <div class="row">
            <span class="glyph"><Icon name="branch" size={14} /></span>
            <div class="tk">
              <span class="mname">GitHub App · chasercat</span>
              <span class="tmeta num">2 repos linked · previews per PR · webhook verified</span>
            </div>
            <span class="grow"></span>
            <span class="led live"></span>
          </div>
        </div>
        <div class="foot num">builds run server-side · lifecycle scripts disabled by default</div>
      </section>
    </div>

    <div class="col">
      <!-- domains -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Domains</span>
          <button class="btn sm" disabled title="Unavailable: daemon admin bridge offline"><Icon name="plus" size={12} />Add domain</button>
        </div>
        <div class="rows pad0">
          <div class="row">
            <span class="glyph"><Icon name="globe" size={14} /></span>
            <div class="tk">
              <span class="mname num">*.{node.domain}</span>
              <span class="tmeta num">apps domain · wildcard DNS-01</span>
            </div>
            <span class="grow"></span>
            <span class="led live"></span>
          </div>
        </div>
      </section>

      <!-- danger -->
      <section class="card dangercard">
        <div class="cardhead"><span class="label danger">Danger</span></div>
        <div class="pad">
          <div class="setrow">
            <div class="settext">
              <span class="mname">Drain node</span>
              <span class="tmeta">Finish in-flight requests, park every cage.</span>
            </div>
            <button class="btn sm danger" disabled title="Unavailable: daemon admin bridge offline">Drain</button>
          </div>
          <div class="setrow">
            <div class="settext">
              <span class="mname">Rotate node key</span>
              <span class="tmeta">Re-seal env secrets under a fresh key.</span>
            </div>
            <button class="btn sm danger" disabled title="Unavailable: daemon admin bridge offline">Rotate</button>
          </div>
        </div>
      </section>
    </div>
  </div>
</div>

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 26px 44px 0;
  }
  .head { margin-bottom: 18px; }
  h1 {
    font-size: 23px;
    font-weight: 650;
    letter-spacing: -0.02em;
  }
  .sub { margin-top: 5px; font-size: 13px; color: var(--ink-3); }

  .grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 18px;
    align-items: start;
  }
  .col { display: flex; flex-direction: column; gap: 18px; }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
  }
  .label.danger { color: #b02c23; }
  .pad0 { padding: 0 10px 8px; }
  .pad { padding: 2px 18px 16px; }

  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 11px 10px;
  }
  .row + .row { border-top: 1px solid var(--line-2); }
  .mavatar {
    width: 26px;
    height: 26px;
    border-radius: 50%;
    color: #fff;
    font-size: 11px;
    font-weight: 700;
    display: grid;
    place-items: center;
    flex: none;
  }
  .mname { font-size: 13px; font-weight: 600; }
  .grow { flex: 1; }
  .glyph {
    width: 28px;
    height: 28px;
    border-radius: 9px;
    background: var(--surface-3);
    color: var(--ink-2);
    display: grid;
    place-items: center;
    flex: none;
  }
  .tk { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .tmeta { font-size: 11px; color: var(--ink-3); }
  .foot {
    padding: 10px 18px 14px;
    font-size: 10.5px;
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
    font-family: var(--mono);
  }

  .setrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 16px;
    padding: 11px 0;
  }
  .setrow + .setrow { border-top: 1px solid var(--line-2); }
  .settext { display: flex; flex-direction: column; gap: 3px; }

  .dangercard { border-color: rgba(221, 63, 52, 0.18); }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
  }
</style>
