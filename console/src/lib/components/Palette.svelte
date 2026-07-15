<script>
  import { ui, go, openApp } from '../stores.svelte.js';
  import { apps } from '../data.js';
  import Icon from './Icon.svelte';
  import Identicon from './Identicon.svelte';

  let q = $state('');
  let sel = $state(0);
  let inputEl = $state(null);

  const SCREENS = [
    { id: 'overview', label: 'Overview' },
    { id: 'deploys', label: 'Deploys' },
    { id: 'observe', label: 'Observe' },
    { id: 'node', label: 'Node' },
    { id: 'settings', label: 'Settings' },
  ];

  const results = $derived.by(() => {
    const query = q.trim().toLowerCase();
    const list = [];

    for (const a of apps) {
      if (!query || a.name.includes(query) || a.domain.includes(query)) {
        list.push({
          kind: 'app',
          title: a.name,
          hint: a.domain,
          app: a,
          run: () => openApp(a.id),
        });
      }
    }
    for (const s of SCREENS) {
      if (!query || s.label.toLowerCase().includes(query)) {
        list.push({ kind: 'screen', title: s.label, hint: 'console', icon: s.id, run: () => go(s.id) });
      }
    }
    const actions = [
      { title: 'Ship a new app', icon: 'ship', run: () => { ui.paletteOpen = false; ui.shipOpen = true; } },
      { title: 'Ship · connect Git', icon: 'branch', run: () => { ui.paletteOpen = false; ui.shipOpen = true; } },
      { title: 'Ship · upload a folder', icon: 'folder', run: () => { ui.paletteOpen = false; ui.shipOpen = true; } },
    ];
    for (const a of actions) {
      if (!query || a.title.toLowerCase().includes(query)) {
        list.push({ kind: 'action', hint: 'action', ...a });
      }
    }
    return list.slice(0, 9);
  });

  $effect(() => {
    if (ui.paletteOpen) {
      q = '';
      sel = 0;
      queueMicrotask(() => inputEl?.focus());
    }
  });

  $effect(() => {
    q; // reset selection when query changes
    sel = 0;
  });

  function onkey(e) {
    if (e.key === 'ArrowDown') {
      e.preventDefault();
      sel = Math.min(sel + 1, results.length - 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      sel = Math.max(sel - 1, 0);
    } else if (e.key === 'Enter') {
      results[sel]?.run();
    } else if (e.key === 'Escape') {
      ui.paletteOpen = false;
    }
  }
</script>

{#if ui.paletteOpen}
  <div
    class="scrim"
    onclick={(e) => { if (e.target === e.currentTarget) ui.paletteOpen = false; }}
    role="presentation"
  >
    <div class="palette" role="dialog" aria-label="Command palette">
      <div class="field">
        <Icon name="search" size={16} />
        <input
          bind:this={inputEl}
          bind:value={q}
          onkeydown={onkey}
          placeholder="Apps, screens, actions…"
          spellcheck="false"
        />
        <kbd>esc</kbd>
      </div>
      <div class="results">
        {#each results as r, i}
          <button
            class="row"
            class:sel={i === sel}
            onmouseenter={() => (sel = i)}
            onclick={r.run}
          >
            {#if r.kind === 'app'}
              <Identicon name={r.title} size={20} />
            {:else}
              <span class="glyph"><Icon name={r.icon} size={15} /></span>
            {/if}
            <span class="title">{r.title}</span>
            {#if r.app}
              <span class="led {r.app.state === 'ready' ? (r.app.env === 'preview' ? 'preview' : 'live') : r.app.state === 'building' ? 'build' : 'cold'}"></span>
            {/if}
            <span class="hint num">{r.hint ?? ''}</span>
            {#if i === sel}<kbd>↵</kbd>{/if}
          </button>
        {/each}
        {#if !results.length}
          <div class="empty">Nothing on this node matches “{q}”.</div>
        {/if}
      </div>
      <div class="foot">
        <span><kbd>↑</kbd><kbd>↓</kbd> navigate</span>
        <span><kbd>↵</kbd> open</span>
        <span class="grow"></span>
        <span class="num">swan-01 · 287 apps</span>
      </div>
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
    justify-content: center;
    animation: scrim-in 0.16s ease both;
  }
  @keyframes scrim-in { from { opacity: 0; } }

  .palette {
    margin-top: 17vh;
    width: min(630px, calc(100vw - 48px));
    height: fit-content;
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 18px;
    box-shadow: var(--shadow-pop);
    overflow: hidden;
    animation: pal-in 0.2s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  @keyframes pal-in {
    from { opacity: 0; transform: translateY(8px) scale(0.985); }
  }

  .field {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 16px 18px;
    border-bottom: 1px solid var(--line-2);
    color: var(--ink-3);
  }
  input {
    flex: 1;
    border: none;
    outline: none;
    background: none;
    font-size: 15px;
    font-family: var(--sans);
    color: var(--ink);
  }
  input::placeholder { color: var(--ink-4); }

  .results { padding: 7px; }
  .row {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 9px 11px;
    border-radius: 10px;
    text-align: left;
  }
  .row.sel { background: var(--surface-3); }
  .glyph {
    width: 20px;
    height: 20px;
    display: grid;
    place-items: center;
    color: var(--ink-3);
  }
  .title {
    font-size: 13.5px;
    font-weight: 550;
    color: var(--ink);
  }
  .hint {
    margin-left: auto;
    font-size: 11px;
    color: var(--ink-3);
  }
  .empty {
    padding: 22px 14px;
    font-size: 13px;
    color: var(--ink-3);
    text-align: center;
  }

  .foot {
    display: flex;
    align-items: center;
    gap: 14px;
    padding: 10px 16px;
    border-top: 1px solid var(--line-2);
    background: var(--surface-2);
    font-size: 11px;
    color: var(--ink-3);
  }
  .foot span { display: inline-flex; gap: 5px; align-items: center; }
  .grow { flex: 1; }
</style>
