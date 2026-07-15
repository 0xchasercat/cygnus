<script>
  import { ui, go } from '../stores.svelte.js';
  import { apps, tenant0 } from '../data.js';
  import Icon from './Icon.svelte';
  import SwanMark from './SwanMark.svelte';

  const app = $derived(apps.find((a) => a.id === ui.appId));

  const crumb = $derived.by(() => {
    if (ui.screen === 'app' && app) return [{ t: app.name }];
    if (ui.screen === 'deploy' && app)
      return [{ t: app.name, go: () => go('app', { appId: app.id }) }, { t: ui.deployId }];
    return null;
  });
</script>

<header class="topbar">
  <div class="inner">
    <div class="left">
      <button class="brand" onclick={() => go('overview')}>
        <SwanMark size={23} />
        <span class="word">CYGNUS</span>
      </button>

      {#if crumb}
        <button class="back" onclick={() => go(ui.screen === 'deploy' ? 'app' : 'overview', { appId: ui.appId })}>
          <Icon name="back" size={15} />
        </button>
        <div class="crumbs num">
          {#each crumb as c, i}
            {#if i > 0}<span class="sep">/</span>{/if}
            {#if c.go}
              <button class="crumb link" onclick={c.go}>{c.t}</button>
            {:else}
              <span class="crumb">{c.t}</span>
            {/if}
          {/each}
        </div>
      {/if}
    </div>

    <div class="right">
      <button class="find" onclick={() => (ui.paletteOpen = true)}>
        <Icon name="search" size={14} />
        <span>Search the node</span>
        <kbd>⌘K</kbd>
      </button>
      <span class="avatar" title="chase · owner">C</span>
    </div>
  </div>
  <div class="tenant-status" role="status" aria-label="Tenant 0 preview dataset; daemon bridge offline">
    <span class="led preview" aria-hidden="true"></span>
    <span class="tenant-id num">{tenant0.id}</span>
    <span>{tenant0.dataSourceLabel}</span>
    <span class="divider" aria-hidden="true">·</span>
    <span>daemon bridge {tenant0.daemonBridge}</span>
  </div>
</header>

<style>
  .topbar {
    position: relative;
    z-index: 10;
    padding: 22px 0 6px;
  }
  .inner {
    max-width: 1264px;
    margin: 0 auto;
    padding: 0 44px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
  }
  .tenant-status {
    max-width: 1264px;
    margin: 7px auto 0;
    padding: 0 44px;
    display: flex;
    align-items: center;
    gap: 7px;
    color: var(--ink-3);
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.025em;
  }
  .tenant-status .led { width: 6px; height: 6px; }
  .tenant-id { color: var(--ink-2); font-weight: 500; }
  .divider { color: var(--ink-4); }
  .left {
    display: flex;
    align-items: center;
    gap: 14px;
    min-height: 36px;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 11px;
    color: var(--ink);
  }
  .word {
    font-size: 13.5px;
    font-weight: 700;
    letter-spacing: 0.26em;
    translate: 0 0.5px;
  }
  .back {
    width: 30px;
    height: 30px;
    border-radius: 9px;
    border: 1px solid var(--line);
    background: var(--surface);
    display: grid;
    place-items: center;
    color: var(--ink-2);
    transition: background 0.12s ease, color 0.12s ease;
  }
  .back:hover { background: var(--surface-3); color: var(--ink); }
  .crumbs {
    display: flex;
    align-items: center;
    gap: 8px;
    font-size: 13px;
    color: var(--ink);
  }
  .crumb.link { color: var(--ink-3); font-family: var(--mono); }
  .crumb.link:hover { color: var(--ink); }
  .crumb { font-family: var(--mono); }
  .sep { color: var(--ink-4); }

  .right {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .find {
    display: flex;
    align-items: center;
    gap: 9px;
    height: 33px;
    padding: 0 6px 0 12px;
    min-width: 210px;
    border: 1px solid var(--line);
    border-radius: 10px;
    background: rgba(255, 255, 255, 0.72);
    color: var(--ink-3);
    font-size: 12.5px;
    transition: border-color 0.13s ease, background 0.13s ease, box-shadow 0.13s ease;
  }
  .find:hover {
    border-color: var(--line-strong);
    background: var(--surface);
    box-shadow: var(--shadow-card);
  }
  .find span { flex: 1; text-align: left; }
  .avatar {
    width: 31px;
    height: 31px;
    border-radius: 50%;
    background: linear-gradient(135deg, var(--cobalt), var(--violet));
    color: #fff;
    font-size: 12px;
    font-weight: 700;
    display: grid;
    place-items: center;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.25), var(--shadow-card);
    margin-left: 2px;
  }
</style>
