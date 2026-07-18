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
  <div class="loading num">LOCATING TENANT ZERO…</div>
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
      {#if store.mode === 'live'}
        {#if store.connected}
          <span class="conn"><span class="led live breathe"></span>connected</span>
        {:else}
          <span class="conn amber"><span class="led build breathe"></span>reconnecting…</span>
        {/if}
      {/if}
    </footer>
  </div>

  <Palette />
  <ShipModal />
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
  .conn {
    display: inline-flex;
    align-items: center;
    gap: 7px;
  }
  .conn .led { width: 6px; height: 6px; }
  .conn.amber { color: var(--amber); }

  .loading {
    min-height: 100vh;
    display: grid;
    place-items: center;
    color: var(--ink-4);
    font-size: 11px;
    letter-spacing: 0.18em;
  }
</style>
