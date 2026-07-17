<script>
  import { ui, go } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import Icon from './Icon.svelte';

  // fleet on the left · machine on the right · shipping in the middle
  const LEFT = [
    { id: 'overview', label: 'Overview', icon: 'overview' },
    { id: 'deploys', label: 'Deploys', icon: 'deploys' },
    { id: 'observe', label: 'Observe', icon: 'observe' },
  ];
  const RIGHT = [
    { id: 'node', label: 'Node', icon: 'node' },
    { id: 'settings', label: 'Settings', icon: 'settings' },
  ];

  // drill-in screens keep Overview lit — it's the fleet context
  const section = $derived(
    ['app', 'deploy'].includes(ui.screen) ? 'overview' : ui.screen
  );

  const building = $derived(store.deployments.some((d) => d.status === 'building'));
</script>

<div class="dock">
  {#each LEFT as item}
    <button
      class="item"
      class:on={section === item.id}
      onclick={() => go(item.id)}
      aria-label={item.label}
    >
      <Icon name={item.icon} size={20} />
      {#if item.id === 'deploys' && building}
        <span class="busy" title="a build is running"></span>
      {/if}
      <span class="tip">{item.label}</span>
    </button>
  {/each}

  <button class="ship" onclick={() => (ui.shipOpen = true)} aria-label="Ship an app">
    <Icon name="plus" size={22} stroke={2.1} />
    <span class="tip">Ship an app</span>
  </button>

  {#each RIGHT as item}
    <button
      class="item"
      class:on={section === item.id}
      onclick={() => go(item.id)}
      aria-label={item.label}
    >
      <Icon name={item.icon} size={20} />
      <span class="tip">{item.label}</span>
    </button>
  {/each}

  <button class="item" onclick={() => (ui.paletteOpen = true)} aria-label="Search">
    <Icon name="search" size={19} />
    <span class="tip">Search<em>⌘K</em></span>
  </button>
</div>

<style>
  .dock {
    position: fixed;
    z-index: 60;
    bottom: 20px;
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    align-items: center;
    gap: 4px;
    padding: 7px 9px;
    background: rgba(255, 255, 255, 0.82);
    backdrop-filter: blur(20px) saturate(1.6);
    -webkit-backdrop-filter: blur(20px) saturate(1.6);
    border: 1px solid var(--line);
    border-radius: 24px;
    box-shadow: var(--shadow-float);
  }

  .item {
    position: relative;
    width: 46px;
    height: 46px;
    border-radius: 15px;
    display: grid;
    place-items: center;
    color: var(--ink-2);
    transition: background 0.14s ease, color 0.14s ease,
      transform 0.16s cubic-bezier(0.34, 1.4, 0.64, 1);
  }
  .item:hover {
    background: #eef0f5;
    color: var(--ink);
    transform: translateY(-1px);
  }
  .item:active { transform: scale(0.94); }
  .item.on {
    background: var(--ink);
    color: #fff;
    box-shadow: 0 6px 16px -6px rgba(12, 15, 20, 0.45);
  }

  /* build-in-progress LED on the Deploys glyph */
  .busy {
    position: absolute;
    top: 9px;
    right: 9px;
    width: 5.5px;
    height: 5.5px;
    border-radius: 50%;
    background: var(--amber);
    box-shadow: 0 0 0 2.5px var(--amber-soft);
    animation: led-breathe 1.6s ease-in-out infinite;
  }
  .item.on .busy { box-shadow: 0 0 0 2.5px rgba(255, 255, 255, 0.18); }

  /* the shutter button — shipping is the center of the product */
  .ship {
    position: relative;
    width: 54px;
    height: 54px;
    margin: 0 7px;
    border-radius: 19px;
    display: grid;
    place-items: center;
    background: var(--cobalt);
    color: #fff;
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.22),
      0 2px 4px rgba(30, 52, 196, 0.25),
      0 10px 26px -8px rgba(44, 70, 240, 0.6);
    transition: background 0.15s ease, transform 0.18s cubic-bezier(0.34, 1.45, 0.64, 1),
      box-shadow 0.18s ease;
  }
  .ship:hover {
    background: var(--cobalt-deep);
    transform: translateY(-2px) scale(1.04);
    box-shadow:
      inset 0 0 0 1px rgba(255, 255, 255, 0.24),
      0 3px 6px rgba(30, 52, 196, 0.28),
      0 16px 34px -10px rgba(44, 70, 240, 0.72);
  }
  .ship:active { transform: scale(0.96); }

  .tip {
    position: absolute;
    pointer-events: none;
    opacity: 0;
    bottom: calc(100% + 12px);
    left: 50%;
    translate: -50% 3px;
    background: var(--ink);
    color: #fff;
    font-size: 11.5px;
    font-weight: 600;
    letter-spacing: 0.01em;
    padding: 5px 9px;
    border-radius: 8px;
    white-space: nowrap;
    transition: opacity 0.12s ease 0.06s, translate 0.12s ease 0.06s;
    display: flex;
    gap: 7px;
    align-items: center;
  }
  .tip em {
    font-style: normal;
    font-family: var(--mono);
    font-size: 10px;
    opacity: 0.55;
  }
  .item:hover .tip,
  .ship:hover .tip {
    opacity: 1;
    translate: -50% 0;
  }
</style>
