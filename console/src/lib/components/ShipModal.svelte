<script>
  import { ui } from '../stores.svelte.js';
  import Icon from './Icon.svelte';

  let mode = $state(null); // null | 'git' | 'local'
  let copied = $state(false);
  let sim = $state(-1); // -1 idle · 0..N-1 running · N done
  let timers = [];

  const SIM_STEPS = [
    { text: 'uploading source · 1.2 MB · 214 files', ms: 900 },
    { text: 'build cage · bun install · lockfile verified', ms: 1400 },
    { text: 'bundle.js + bundle.jsc sealed · content-addressed', ms: 1100 },
    { text: 'blue-green swap · route armed', ms: 700 },
  ];

  const repos = [
    { name: 'chasercat/atelier', state: 'linked' },
    { name: 'chasercat/helios', state: 'linked' },
    { name: 'chasercat/labs', state: 'import' },
  ];

  function copy() {
    copied = true;
    setTimeout(() => (copied = false), 1400);
  }

  function startSim() {
    if (sim >= 0) return;
    sim = 0;
    let acc = 0;
    SIM_STEPS.forEach((s, i) => {
      acc += s.ms;
      timers.push(setTimeout(() => (sim = i + 1), acc));
    });
  }

  function reset() {
    mode = null;
    sim = -1;
    timers.forEach(clearTimeout);
    timers = [];
  }

  // clean slate whenever the modal closes
  $effect(() => {
    if (!ui.shipOpen) reset();
  });

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
          {#if mode}
            <button class="backlink" onclick={reset} aria-label="Back">
              <Icon name="back" size={14} />
            </button>
          {/if}
          <div>
            <h2>Ship to swan-01</h2>
            <p>Source goes up. The node builds, seals the artifact, swaps blue-green.</p>
          </div>
        </div>
        <button class="btn icon sm" onclick={() => (ui.shipOpen = false)} aria-label="Close">
          <Icon name="x" size={14} />
        </button>
      </header>

      {#if !mode}
        <!-- ————— the two ways in ————— -->
        <div class="choices">
          <button class="choice" onclick={() => (mode = 'git')}>
            <span class="cicon"><Icon name="branch" size={19} /></span>
            <span class="cname">Connect Git</span>
            <span class="cdesc">Push-to-deploy on linked repos. A preview cage per pull request.</span>
            <span class="cgo"><Icon name="arrowR" size={14} /></span>
          </button>
          <button class="choice" onclick={() => (mode = 'local')}>
            <span class="cicon"><Icon name="folder" size={19} /></span>
            <span class="cname">Upload a folder</span>
            <span class="cdesc">Drop your project. No image, no registry — source only, megabytes.</span>
            <span class="cgo"><Icon name="arrowR" size={14} /></span>
          </button>
        </div>
      {:else if mode === 'git'}
        <div class="git">
          <div class="gh">
            <span class="ghmark"><Icon name="branch" size={15} /></span>
            <div class="ghtext">
              <b>GitHub App connected</b>
              <span>chasercat · webhook signature-verified by Tenant 0</span>
            </div>
            <span class="led live"></span>
          </div>
          <div class="rows">
            {#each repos as r}
              <div class="repo">
                <span class="num rname">{r.name}</span>
                {#if r.state === 'linked'}
                  <span class="pill live">deploying on push</span>
                {:else}
                  <button class="btn sm">Import</button>
                {/if}
              </div>
            {/each}
          </div>
        </div>
      {:else}
        <!-- ————— local folder ————— -->
        {#if sim === -1}
          <button class="drop" onclick={startSim}>
            <span class="dicon"><Icon name="deploys" size={22} /></span>
            <span class="dname">Drop a folder</span>
            <span class="ddesc">or click to browse · the node builds it in an ephemeral cage</span>
          </button>
        {:else}
          <div class="run">
            {#each SIM_STEPS as s, i}
              {#if sim >= i}
                <div class="rline" class:done={sim > i}>
                  {#if sim > i}
                    <span class="rdot ok"><Icon name="check" size={9} stroke={2.8} /></span>
                  {:else}
                    <span class="rdot spin"></span>
                  {/if}
                  <span class="rtext num">{s.text}</span>
                </div>
              {/if}
            {/each}
            {#if sim >= SIM_STEPS.length}
              <div class="livebox">
                <span class="led live breathe"></span>
                <span class="liveurl num">live · https://my-app.swan.host</span>
                <span class="livems num">revives in ≈34 ms</span>
              </div>
            {/if}
          </div>
        {/if}
      {/if}

      <footer>
        <span class="fcli num"><i>$</i> cygnus deploy</span>
        <button class="fcopy" onclick={copy}>
          <Icon name={copied ? 'check' : 'copy'} size={12} />
          {copied ? 'copied' : 'copy'}
        </button>
        <span class="fnote">or ship from a terminal — same pipeline, same node</span>
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
  .backlink {
    width: 28px;
    height: 28px;
    margin-top: 1px;
    border-radius: 9px;
    border: 1px solid var(--line);
    display: grid;
    place-items: center;
    color: var(--ink-2);
    transition: background 0.12s ease;
  }
  .backlink:hover { background: var(--surface-3); }
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

  /* ————— git ————— */
  .gh {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 13px 14px;
    border: 1px solid var(--line);
    border-radius: 13px;
    background: var(--surface-2);
    margin-bottom: 12px;
  }
  .ghmark {
    width: 32px;
    height: 32px;
    border-radius: 10px;
    background: var(--ink);
    color: #fff;
    display: grid;
    place-items: center;
  }
  .ghtext { flex: 1; display: flex; flex-direction: column; gap: 1px; }
  .ghtext b { font-size: 13px; font-weight: 650; }
  .ghtext span { font-size: 11.5px; color: var(--ink-3); }
  .repo {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 11px 6px;
  }
  .repo + .repo { border-top: 1px solid var(--line-2); }
  .rname { font-size: 12.5px; color: var(--ink); }

  /* ————— local folder ————— */
  .drop {
    width: 100%;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: 7px;
    padding: 34px 20px 30px;
    border: 1.5px dashed var(--line-strong);
    border-radius: 16px;
    color: var(--ink-2);
    transition: border-color 0.15s ease, background 0.15s ease, color 0.15s ease;
  }
  .drop:hover {
    border-color: var(--cobalt);
    background: var(--cobalt-ghost);
    color: var(--cobalt-deep);
  }
  .dicon {
    width: 44px;
    height: 44px;
    border-radius: 14px;
    background: var(--surface);
    border: 1px solid var(--line);
    display: grid;
    place-items: center;
    margin-bottom: 4px;
  }
  .dname { font-size: 14.5px; font-weight: 650; color: var(--ink); }
  .ddesc { font-size: 11.5px; color: var(--ink-3); }

  .run {
    border: 1px solid var(--line-2);
    background: var(--surface-3);
    border-radius: 16px;
    padding: 18px 20px;
    display: flex;
    flex-direction: column;
    gap: 11px;
    min-height: 148px;
  }
  .rline {
    display: flex;
    align-items: center;
    gap: 11px;
    animation: req-in 0.25s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  @keyframes req-in {
    from { opacity: 0; transform: translateY(-3px); }
  }
  .rdot {
    width: 17px;
    height: 17px;
    border-radius: 50%;
    flex: none;
    display: grid;
    place-items: center;
  }
  .rdot.ok { background: var(--live); color: #fff; }
  .rdot.spin {
    border: 2px solid var(--line-strong);
    border-top-color: var(--cobalt);
    animation: spin 0.7s linear infinite;
  }
  @keyframes spin { to { transform: rotate(360deg); } }
  .rtext { font-size: 12px; color: var(--ink-2); }
  .rline.done .rtext { color: var(--ink-3); }

  .livebox {
    display: flex;
    align-items: center;
    gap: 10px;
    margin-top: 3px;
    padding: 11px 13px;
    background: var(--live-soft);
    border-radius: 11px;
    animation: req-in 0.3s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  .liveurl { font-size: 12px; font-weight: 600; color: #087a45; flex: 1; }
  .livems { font-size: 11px; color: #087a45; opacity: 0.75; }

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
  .fcopy {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    height: 24px;
    padding: 0 9px;
    border: 1px solid var(--line);
    border-radius: 8px;
    background: var(--surface);
    font-family: var(--mono);
    font-size: 10.5px;
    color: var(--ink-2);
    transition: background 0.12s ease;
  }
  .fcopy:hover { background: var(--surface-3); }
  .fnote {
    margin-left: auto;
    font-size: 11px;
    color: var(--ink-4);
  }
</style>
