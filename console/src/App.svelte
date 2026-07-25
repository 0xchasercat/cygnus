<script>
  import { onMount } from 'svelte';
  import { ui } from './lib/stores.svelte.js';
  import { store } from './lib/live.svelte.js';
  import Nav from './lib/components/Nav.svelte';
  import TopBar from './lib/components/TopBar.svelte';
  import Palette from './lib/components/Palette.svelte';
  import ShipModal from './lib/components/ShipModal.svelte';
  import Login from './lib/screens/Login.svelte';
  import Setup from './lib/screens/Setup.svelte';
  import Overview from './lib/screens/Overview.svelte';
  import AppDetail from './lib/screens/AppDetail.svelte';
  import DeployDetail from './lib/screens/DeployDetail.svelte';
  import Deploys from './lib/screens/Deploys.svelte';
  import Observe from './lib/screens/Observe.svelte';
  import NodeScreen from './lib/screens/NodeScreen.svelte';
  import SettingsScreen from './lib/screens/SettingsScreen.svelte';
  import Icon from './lib/components/Icon.svelte';

  const SCREENS = {
    overview: Overview,
    app: AppDetail,
    deploy: DeployDetail,
    deploys: Deploys,
    observe: Observe,
    node: NodeScreen,
    settings: SettingsScreen,
  };

  const Screen = $derived(SCREENS[ui.screen] ?? Overview);
  const screenKey = $derived(`${ui.screen}·${ui.appId ?? ''}·${ui.deployId ?? ''}`);

  // One shell for live and preview. Setup owns live+setup; Login owns
  // live+!ready (signin/locked); everything else renders the polished screens.
  const ready = $derived(store.mode !== 'loading' && store.auth === 'ready');
  const needsSetup = $derived(store.mode !== 'loading' && store.auth === 'setup');

  const footer = $derived.by(() => {
    if (store.mode === 'preview') {
      return 'preview dataset · cygnus 0.9.2 · no daemon connection';
    }
    if (store.mode === 'live') {
      const v = store.node?.version ?? 'dev';
      const host = store.node?.apps_domain ?? store.node?.listen ?? '—';
      return `cygnus ${v} · ${host}`;
    }
    return '';
  });

  onMount(() => store.boot());

  // Global toast — auto-dismisses after 4.5s, cleared on change/unmount.
  let _toastTimer = null;
  $effect(() => {
    const msg = store.notice;
    if (_toastTimer) { clearTimeout(_toastTimer); _toastTimer = null; }
    if (msg) {
      _toastTimer = setTimeout(() => { store.notice = ''; }, 4500);
    }
    return () => { if (_toastTimer) { clearTimeout(_toastTimer); _toastTimer = null; } };
  });

  function onKeydown(e) {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'k') {
      e.preventDefault();
      ui.paletteOpen = !ui.paletteOpen;
      ui.shipOpen = false;
    }
    if (e.key === 'Escape') {
      ui.paletteOpen = false;
      ui.shipOpen = false;
    }
  }
</script>

<svelte:window onkeydown={onKeydown} />

{#if store.mode === 'loading'}
  <div class="loading num">CONNECTING TO NODE…</div>
{:else if needsSetup}
  <Setup />
{:else if store.mode === 'live' && !ready}
  <Login />
{:else}
  <div class="canvas-marks"></div>

  <div class="shell">
    <Nav />
    <TopBar />
    <main>
      {#key screenKey}
        <Screen />
      {/key}
    </main>

    <footer class="colophon num">
      <span>{footer}</span>
    </footer>
  </div>

  <Palette />
  <ShipModal />

  {#if store.notice}
    <div class="global-toast" role="status" aria-live="polite">
      <Icon name="check" size={13} />
      <span class="toast-msg">{store.notice}</span>
      <button class="toast-dismiss" onclick={() => (store.notice = '')} aria-label="Dismiss">✕</button>
    </div>
  {/if}
{/if}

<style>
  .shell {
    position: relative;
    z-index: 1;
    min-height: 100vh;
  }

  main { padding-bottom: 40px; }

  .colophon {
    text-align: center;
    padding: 28px 0 116px; /* clearance for the dock */
    font-size: 10.5px;
    letter-spacing: 0.08em;
    color: var(--ink-4);
    display: flex;
    justify-content: center;
    gap: 12px;
    align-items: center;
  }

  .loading {
    min-height: 100vh;
    display: grid;
    place-items: center;
    color: var(--ink-4);
    font-size: 11px;
    letter-spacing: 0.18em;
  }

  .global-toast {
    position: fixed;
    bottom: 28px;
    right: 28px;
    z-index: 200;
    display: flex;
    align-items: center;
    gap: 8px;
    padding: 10px 14px;
    border: 1px solid color-mix(in srgb, var(--live) 35%, var(--line));
    border-radius: 11px;
    background: var(--live-soft);
    color: #087a45;
    font-size: 12px;
    box-shadow: 0 4px 16px rgba(0, 0, 0, 0.12);
    animation: toast-in 0.22s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  @keyframes toast-in {
    from { opacity: 0; transform: translateY(8px); }
  }
  .toast-msg { flex: 1; }
  .toast-dismiss {
    background: none;
    border: 0;
    padding: 0 0 0 6px;
    color: #087a45;
    cursor: pointer;
    font-size: 12px;
    line-height: 1;
    opacity: 0.7;
  }
  .toast-dismiss:hover { opacity: 1; }
</style>
