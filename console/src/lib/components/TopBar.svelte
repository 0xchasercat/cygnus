<script>
  import { ui, go } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import Icon from './Icon.svelte';
  import SwanMark from './SwanMark.svelte';

  const app = $derived(store.appByName(ui.appId) ?? store.apps.find((a) => a.name === ui.appId) ?? null);

  const crumb = $derived.by(() => {
    if (ui.screen === 'app' && app) return [{ t: app.name }];
    if (ui.screen === 'deploy' && app)
      return [{ t: app.name, go: () => go('app', { appId: app.name }) }, { t: ui.deployId }];
    return null;
  });

  const tenantLine = $derived.by(() => {
    if (store.mode === 'live') {
      const host = store.node?.apps_domain ?? store.node?.listen ?? '—';
      return { id: 'tenant zero', mid: 'live', tail: host, live: true };
    }
    return { id: 'tenant zero', mid: 'preview', tail: 'cygnus 0.9.2', live: false };
  });

  let menuOpen = $state(false);

  function toggleMenu(e) {
    e.stopPropagation();
    menuOpen = !menuOpen;
  }
  function closeMenu() {
    menuOpen = false;
  }
  async function signOut() {
    menuOpen = false;
    await store.signOut();
  }
  $effect(() => {
    if (menuOpen) {
      window.addEventListener('click', closeMenu, { once: true });
      return () => window.removeEventListener('click', closeMenu);
    }
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
      <button class="avatar" onclick={toggleMenu} aria-haspopup="menu" aria-expanded={menuOpen} title="operator">
        <span class="av-t">OP</span>
      </button>
      {#if menuOpen}
        <div class="popover" role="menu" onclick={(e) => e.stopPropagation()}>
          <div class="pop-head">
            <span class="pop-label mono">operator</span>
          </div>
          {#if store.mode === 'live'}
            <button class="pop-item" role="menuitem" onclick={signOut}>
              <Icon name="x" size={13} /> Sign out
            </button>
          {:else}
            <span class="pop-empty">preview dataset · no session</span>
          {/if}
        </div>
      {/if}
    </div>
  </div>
  <div class="tenant-status" role="status" aria-label="Tenant status">
    <span class="led {tenantLine.live ? (store.connected ? 'live' : 'build') : 'preview'} breathe" aria-hidden="true"></span>
    <span class="tenant-id num">{tenantLine.id}</span>
    <span>{tenantLine.mid}</span>
    <span class="divider" aria-hidden="true">·</span>
    <span>{tenantLine.tail}</span>
    {#if store.mode === 'live' && !store.connected}
      <span class="divider" aria-hidden="true">·</span>
      <span class="amber">reconnecting…</span>
    {/if}
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
    display: grid;
    place-items: center;
    box-shadow: inset 0 0 0 1px rgba(255, 255, 255, 0.25), var(--shadow-card);
    margin-left: 2px;
  }
  .av-t {
    font-size: 9.5px;
    font-weight: 700;
    font-family: var(--mono);
    letter-spacing: 0.06em;
  }
  .popover {
    position: absolute;
    top: calc(50% + 22px);
    right: 0;
    min-width: 168px;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 12px;
    box-shadow: var(--shadow-pop);
    padding: 6px;
    z-index: 80;
    animation: pop-in 0.14s ease both;
  }
  @keyframes pop-in {
    from { opacity: 0; transform: translateY(-4px) scale(0.98); }
  }
  .pop-head {
    padding: 7px 9px 6px;
    border-bottom: 1px solid var(--line-2);
    margin-bottom: 5px;
  }
  .pop-label {
    font-size: 10px;
    color: var(--ink-3);
    letter-spacing: 0.08em;
    text-transform: uppercase;
  }
  .pop-item {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 8px 9px;
    border-radius: 8px;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--ink);
    text-align: left;
  }
  .pop-item:hover { background: var(--surface-3); }
  .pop-empty {
    display: block;
    padding: 8px 9px;
    font-size: 11px;
    color: var(--ink-4);
    font-family: var(--mono);
  }
  .amber { color: var(--amber); }
  .right { position: relative; }
</style>
