<script>
  import { ui } from '../stores.svelte.js';
  import Icon from './Icon.svelte';

  // Tenant 0 has no typed daemon admin bridge, so shipping stays read-only.
  function onkey(e) {
    if (e.key === 'Escape') ui.shipOpen = false;
  }

</script>

<svelte:window onkeydown={ui.shipOpen ? onkey : undefined} />

{#if ui.shipOpen}
  <div
    class="scrim"
    onclick={(e) => { if (e.target === e.currentTarget) ui.shipOpen = false; }}
    role="presentation"
  >
    <div class="modal" role="dialog" aria-label="Ship an app">
      <header>
        <div class="htitle">
          <div>
            <h2>Ship to swan-01</h2>
            <p>Tenant 0 is in preview mode. The daemon admin bridge is offline.</p>
          </div>
        </div>
        <button class="btn icon sm" onclick={() => (ui.shipOpen = false)} aria-label="Close">
          <Icon name="x" size={14} />
        </button>
      </header>

      <div class="choices" aria-describedby="ship-offline-note">
        <button class="choice" disabled title="Unavailable: daemon admin bridge offline">
          <span class="cicon"><Icon name="branch" size={19} /></span>
          <span class="cname">Connect Git</span>
          <span class="cdesc">Push-to-deploy and pull-request previews require the typed admin bridge.</span>
          <span class="cgo"><Icon name="arrowR" size={14} /></span>
        </button>
        <button class="choice" disabled title="Unavailable: daemon admin bridge offline">
          <span class="cicon"><Icon name="folder" size={19} /></span>
          <span class="cname">Upload a folder</span>
          <span class="cdesc">Source upload and builds are unavailable in this read-only console.</span>
          <span class="cgo"><Icon name="arrowR" size={14} /></span>
        </button>
      </div>

      <div id="ship-offline-note" class="offline-note" role="status">
        <span class="led preview" aria-hidden="true"></span>
        <div>
          <b>Preview dataset · daemon bridge offline</b>
          <span>No deploy, import, or upload was started. Connect a typed admin bridge to enable mutations.</span>
        </div>
      </div>

      <footer>
        <span class="fcli num"><i>$</i> tenant 0 · preview</span>
        <span class="fnote">Deploy, import, and upload are disabled · daemon admin bridge offline</span>
      </footer>
    </div>
  </div>
{/if}

<style>
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: rgba(12, 15, 20, 0.3);
    backdrop-filter: blur(7px) saturate(1.1);
    -webkit-backdrop-filter: blur(7px) saturate(1.1);
    display: flex;
    align-items: flex-start;
    justify-content: center;
    animation: scrim-in 0.16s ease both;
  }
  @keyframes scrim-in { from { opacity: 0; } }

  .modal {
    margin-top: 15vh;
    width: min(620px, calc(100vw - 48px));
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 22px;
    box-shadow: var(--shadow-pop);
    padding: 26px 26px 0;
    animation: pal-in 0.22s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  @keyframes pal-in {
    from { opacity: 0; transform: translateY(10px) scale(0.985); }
  }

  header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    margin-bottom: 20px;
  }
  .htitle { display: flex; gap: 12px; align-items: flex-start; }
  h2 {
    font-size: 18px;
    font-weight: 650;
    letter-spacing: -0.015em;
  }
  header p {
    margin-top: 5px;
    font-size: 13px;
    color: var(--ink-2);
  }

  /* ————— chooser ————— */
  .choices {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
  }
  .choice {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 8px;
    padding: 18px 18px 16px;
    border: 1px solid var(--line);
    border-radius: 16px;
    background: var(--surface);
    text-align: left;
    transition: border-color 0.14s ease, box-shadow 0.14s ease, transform 0.14s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .choice:hover {
    border-color: var(--cobalt);
    box-shadow: 0 0 0 3px var(--cobalt-ghost), var(--shadow-hover);
    transform: translateY(-1px);
  }
  .cicon {
    width: 38px;
    height: 38px;
    border-radius: 12px;
    background: var(--surface-3);
    color: var(--ink);
    display: grid;
    place-items: center;
    margin-bottom: 2px;
  }
  .choice:hover .cicon { background: var(--cobalt-ghost); color: var(--cobalt-deep); }
  .cname { font-size: 14.5px; font-weight: 650; letter-spacing: -0.01em; }
  .cdesc { font-size: 12px; line-height: 1.55; color: var(--ink-3); }
  .cgo {
    position: absolute;
    top: 16px;
    right: 14px;
    color: var(--ink-4);
    transition: color 0.14s ease, translate 0.14s ease;
  }
  .choice:hover .cgo { color: var(--cobalt); translate: 2px 0; }
  .choice:disabled {
    cursor: not-allowed;
    opacity: 0.7;
  }
  .choice:disabled:hover {
    border-color: var(--line);
    box-shadow: none;
    transform: none;
  }
  .choice:disabled .cgo { color: var(--ink-4); }
  .choice:disabled .cicon { color: var(--ink-3); }


  .offline-note {
    display: flex;
    align-items: flex-start;
    gap: 10px;
    margin-top: 15px;
    padding: 12px 13px;
    border: 1px solid var(--violet-soft);
    border-radius: 11px;
    background: var(--violet-soft);
    color: var(--ink-2);
  }
  .offline-note .led { margin-top: 5px; }
  .offline-note div { display: flex; flex-direction: column; gap: 2px; }
  .offline-note b { font-size: 12px; font-weight: 650; color: var(--ink); }
  .offline-note span:not(.led) { font-size: 11.5px; line-height: 1.45; }
  /* ————— cli footer ————— */
  footer {
    display: flex;
    align-items: center;
    gap: 10px;
    margin: 20px -26px 0;
    padding: 13px 26px;
    border-top: 1px solid var(--line-2);
    background: var(--surface-2);
    border-radius: 0 0 22px 22px;
  }
  .fcli { font-size: 12px; color: var(--ink); }
  .fcli i { font-style: normal; color: var(--ink-4); margin-right: 7px; }
  .fnote {
    margin-left: auto;
    font-size: 11px;
    color: var(--ink-4);
  }
</style>
