<script>
  import { onMount } from 'svelte';
  import { ui } from './lib/stores.svelte.js';
  import Nav from './lib/components/Nav.svelte';
  import TopBar from './lib/components/TopBar.svelte';
  import Palette from './lib/components/Palette.svelte';
  import ShipModal from './lib/components/ShipModal.svelte';
  import Overview from './lib/screens/Overview.svelte';
  import AppDetail from './lib/screens/AppDetail.svelte';
  import DeployDetail from './lib/screens/DeployDetail.svelte';
  import Deploys from './lib/screens/Deploys.svelte';
  import Observe from './lib/screens/Observe.svelte';
  import NodeScreen from './lib/screens/NodeScreen.svelte';
  import SettingsScreen from './lib/screens/SettingsScreen.svelte';
  import LiveConsole from './lib/screens/LiveConsole.svelte';

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
  let dataMode = $state('loading');

  onMount(async () => {
    try {
      const response = await fetch('/healthz', { headers: { accept: 'application/json' } });
      const health = await response.json();
      dataMode = response.ok && health.mode === 'live' ? 'live' : 'preview';
    } catch {
      dataMode = 'preview';
    }
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

{#if dataMode === 'live'}
  <LiveConsole />
{:else if dataMode === 'preview'}
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
      preview dataset · cygnus 0.9.2 · no daemon connection
    </footer>
  </div>

  <Palette />
  <ShipModal />
{:else}
  <div class="loading num">LOCATING TENANT ZERO…</div>
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
  }
</style>
