<script>
  import { onMount } from 'svelte';

  let loading = $state(true);
  let error = $state('');
  let status = $state(null);
  let apps = $state([]);
  let deployments = $state([]);

  async function load() {
    loading = true;
    error = '';
    try {
      const [statusData, appsData, deploymentsData] = await Promise.all([
        api('/api/v1/status'),
        api('/api/v1/apps?limit=50'),
        api('/api/v1/deployments?limit=50'),
      ]);
      status = statusData.node;
      apps = appsData.apps;
      deployments = deploymentsData.deployments;
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'Daemon bridge unavailable';
    } finally {
      loading = false;
    }
  }

  async function api(path) {
    const response = await fetch(path, { headers: { accept: 'application/json' } });
    const envelope = await response.json().catch(() => null);
    if (!response.ok || !envelope?.ok) {
      throw new Error(envelope?.error?.message || `Request failed (${response.status})`);
    }
    return envelope.data;
  }

  function shortHash(value) {
    return value ? `${value.slice(0, 10)}…${value.slice(-6)}` : '—';
  }

  onMount(load);
</script>

<svelte:head><title>Cygnus · Tenant Zero</title></svelte:head>

<div class="live-shell">
  <header class="mast">
    <div>
      <p class="eyebrow">TENANT ZERO · LIVE CONTROL PLANE</p>
      <h1>Cygnus</h1>
      <p class="lede">Daemon state, directly from the mounted typed admin endpoint.</p>
    </div>
    <div class="mast-actions">
      <span class:down={!!error} class="bridge"><i></i>{error ? 'bridge unavailable' : 'daemon connected'}</span>
      <button onclick={load} disabled={loading}>{loading ? 'Refreshing…' : 'Refresh'}</button>
    </div>
  </header>

  {#if error}
    <section class="fault" role="alert">
      <strong>Live state unavailable.</strong>
      <span>{error}</span>
    </section>
  {:else if loading}
    <section class="loading" aria-live="polite">Reading daemon state…</section>
  {:else}
    <section class="metrics" aria-label="Node summary">
      <article><span>Listener</span><strong class="num">{status?.listen ?? '—'}</strong></article>
      <article><span>Registered apps</span><strong class="num">{status?.app_count ?? apps.length}</strong></article>
      <article><span>Ready</span><strong class="num">{apps.filter((app) => app.lifecycle_state === 'ready').length}</strong></article>
      <article><span>Retained deploys</span><strong class="num">{deployments.length}</strong></article>
    </section>

    <section class="panel">
      <div class="panel-head">
        <div><p class="eyebrow">RUNTIME FLEET</p><h2>Apps</h2></div>
        <span class="count num">{apps.length} shown</span>
      </div>
      {#if apps.length === 0}
        <p class="empty">No apps are registered.</p>
      {:else}
        <div class="table-wrap">
          <table>
            <thead><tr><th>App</th><th>State</th><th>Routes</th><th>Policy</th><th>Active artifact</th></tr></thead>
            <tbody>
              {#each apps as app (app.name)}
                <tr>
                  <td><strong>{app.name}</strong><small>{app.env_keys.length} env keys</small></td>
                  <td><span class="state state-{app.lifecycle_state}"><i></i>{app.lifecycle_state}</span></td>
                  <td>
                    {#if app.domains.length}
                      {#each app.domains as domain}<code>{domain}</code>{/each}
                    {:else}<span class="muted">unrouted</span>{/if}
                  </td>
                  <td><span>{app.pinned ? 'pinned' : `${Math.round(app.idle_ttl_ms / 60000)}m idle`}</span><small>{app.egress} egress</small></td>
                  <td class="num">{shortHash(app.active?.artifact_hash)}</td>
                </tr>
              {/each}
            </tbody>
          </table>
        </div>
      {/if}
    </section>

    <section class="panel">
      <div class="panel-head">
        <div><p class="eyebrow">IMMUTABLE HISTORY</p><h2>Deployments</h2></div>
        <span class="count num">latest {deployments.length}</span>
      </div>
      {#if deployments.length === 0}
        <p class="empty">No retained deployments.</p>
      {:else}
        <div class="deploy-grid">
          {#each deployments as deployment (deployment.id)}
            <article>
              <div><strong>{deployment.app}</strong><span class="state state-{deployment.status}"><i></i>{deployment.status}</span></div>
              <code>{deployment.id}</code>
              <p class="num">{shortHash(deployment.artifact_hash)} · {deployment.engine_version}</p>
              {#if deployment.error}<p class="deploy-error">{deployment.error}</p>{/if}
            </article>
          {/each}
        </div>
      {/if}
    </section>

    <footer>
      Tenant Zero writes remain closed until an authenticated actor is available. Use <code>cygnusctl</code> on the host for audited map-domain and rollback operations.
    </footer>
  {/if}
</div>

<style>
  .live-shell { width: min(1380px, calc(100% - 48px)); margin: 0 auto; padding: 44px 0 72px; color: var(--ink-1); }
  .mast { display: flex; justify-content: space-between; gap: 32px; align-items: flex-end; border-bottom: 1px solid var(--line); padding-bottom: 28px; }
  .eyebrow { margin: 0 0 8px; color: var(--blue); font: 600 10px/1.2 var(--mono); letter-spacing: .15em; }
  h1 { margin: 0; font-size: clamp(48px, 7vw, 88px); line-height: .86; letter-spacing: -.065em; }
  h2 { margin: 0; font-size: 28px; letter-spacing: -.035em; }
  .lede { margin: 18px 0 0; color: var(--ink-3); font-size: 14px; }
  .mast-actions { display: flex; align-items: center; gap: 12px; }
  button { border: 1px solid var(--line-2); background: var(--paper); color: var(--ink-1); border-radius: 8px; padding: 10px 14px; font: 600 11px var(--mono); cursor: pointer; }
  button:disabled { opacity: .5; cursor: wait; }
  .bridge, .state { display: inline-flex; align-items: center; gap: 7px; white-space: nowrap; font: 600 10px var(--mono); text-transform: uppercase; letter-spacing: .06em; }
  .bridge i, .state i { width: 6px; height: 6px; border-radius: 50%; background: var(--green); }
  .bridge.down i, .state-failed i { background: var(--red); }
  .state-cold i, .state-sealed i { background: var(--ink-4); }
  .state-building i, .state-booting i { background: var(--amber); }
  .metrics { display: grid; grid-template-columns: repeat(4, 1fr); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; margin: 34px 0 18px; }
  .metrics article { padding: 18px 20px; border-right: 1px solid var(--line); }
  .metrics article:last-child { border: 0; }
  .metrics span, small { display: block; color: var(--ink-4); font-size: 10px; }
  .metrics strong { display: block; margin-top: 8px; font-size: 19px; }
  .panel { border: 1px solid var(--line); border-radius: 12px; margin-top: 18px; overflow: hidden; background: color-mix(in srgb, var(--paper) 94%, transparent); }
  .panel-head { display: flex; align-items: end; justify-content: space-between; padding: 22px 24px; border-bottom: 1px solid var(--line); }
  .count { color: var(--ink-4); font-size: 10px; }
  .table-wrap { overflow-x: auto; }
  table { width: 100%; border-collapse: collapse; text-align: left; }
  th { color: var(--ink-4); font: 500 9px var(--mono); text-transform: uppercase; letter-spacing: .1em; }
  th, td { padding: 14px 18px; border-bottom: 1px solid var(--line); vertical-align: top; }
  tbody tr:last-child td { border-bottom: 0; }
  td { font-size: 12px; }
  td strong { display: block; margin-bottom: 5px; }
  td code { display: block; margin-bottom: 4px; }
  code, .num { font-family: var(--mono); }
  code { color: var(--ink-2); font-size: 10px; }
  .muted, .empty, footer, .loading { color: var(--ink-4); }
  .empty, .loading { padding: 28px 24px; margin: 0; }
  .deploy-grid { display: grid; grid-template-columns: repeat(3, 1fr); }
  .deploy-grid article { padding: 18px; border-right: 1px solid var(--line); border-bottom: 1px solid var(--line); }
  .deploy-grid article > div { display: flex; justify-content: space-between; gap: 12px; margin-bottom: 16px; }
  .deploy-grid p { margin: 10px 0 0; color: var(--ink-3); font-size: 10px; }
  .deploy-grid .deploy-error { color: var(--red); }
  .fault { margin-top: 34px; border: 1px solid color-mix(in srgb, var(--red) 35%, var(--line)); border-radius: 12px; padding: 22px; display: flex; gap: 12px; flex-direction: column; }
  .fault span { color: var(--ink-3); }
  footer { margin-top: 22px; font-size: 11px; line-height: 1.6; }
  @media (max-width: 900px) { .live-shell { width: min(100% - 28px, 720px); padding-top: 28px; } .mast { align-items: flex-start; flex-direction: column; } .metrics { grid-template-columns: repeat(2, 1fr); } .metrics article:nth-child(2) { border-right: 0; } .deploy-grid { grid-template-columns: 1fr; } .deploy-grid article { border-right: 0; } }
  @media (max-width: 560px) { .mast-actions { width: 100%; justify-content: space-between; } .metrics { grid-template-columns: 1fr; } .metrics article { border-right: 0; border-bottom: 1px solid var(--line); } th:nth-child(4), td:nth-child(4), th:nth-child(5), td:nth-child(5) { display: none; } }
</style>
