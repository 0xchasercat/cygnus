<script>
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
    cygnus 0.9.2 · AGPL-3.0 · your hardware, your swan
  </footer>
</div>

<Palette />
<ShipModal />

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
