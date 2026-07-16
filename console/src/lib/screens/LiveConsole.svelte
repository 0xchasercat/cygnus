<script>
  import { onMount } from 'svelte';

  let phase = $state('loading');
  let loading = $state(true);
  let error = $state('');
  let success = $state('');
  let authError = $state('');
  let status = $state(null);
  let apps = $state([]);
  let deployments = $state([]);
  let token = $state('');
  let tokenInput = $state();
  let deployOpen = $state(false);
  let submitting = $state('');
  let rollbackTarget = $state(null);
  let deployedRoute = $state('');
  let mapDomains = $state({});
  let mapErrors = $state({});
  let deployError = $state('');
  let deploy = $state({
    source_dir: '',
    app: '',
    domain: '',
    engine_version: '',
    entry: 'index.ts',
    artifact_root: '',
    upstream: '',
  });

  class ApiError extends Error {
    constructor(message, status, code) {
      super(message);
      this.status = status;
      this.code = code;
    }
  }

  async function api(path, options = {}) {
    const response = await fetch(path, {
      credentials: 'same-origin',
      ...options,
      headers: {
        accept: 'application/json',
        ...(options.body ? { 'content-type': 'application/json' } : {}),
        ...(options.headers ?? {}),
      },
    });
    const envelope = await response.json().catch(() => null);
    if (!response.ok || !envelope?.ok) {
      throw new ApiError(
        envelope?.error?.message || `Request failed (${response.status})`,
        response.status,
        envelope?.error?.code,
      );
    }
    return envelope.data;
  }

  async function boot() {
    loading = true;
    error = '';
    try {
      const session = await api('/api/v1/session');
      if (session.locked || !session.configured) {
        phase = 'locked';
      } else if (!session.authenticated) {
        phase = 'signin';
        focusToken();
      } else {
        phase = 'ready';
        await load();
      }
    } catch (cause) {
      phase = 'locked';
      error = cause instanceof Error ? cause.message : 'Console state unavailable';
    } finally {
      loading = false;
    }
  }

  async function signIn(event) {
    event.preventDefault();
    if (submitting) return;
    authError = '';
    success = '';
    submitting = 'signin';
    try {
      const session = await api('/api/v1/session', {
        method: 'POST',
        body: JSON.stringify({ token }),
      });
      token = '';
      phase = 'ready';
      success = 'Signed in. Reading the node…';
      await load();
      focusMain();
      if (!session?.authenticated) phase = 'signin';
    } catch (cause) {
      authError = cause instanceof Error ? cause.message : 'Sign-in failed';
      token = '';
      focusToken();
    } finally {
      submitting = '';
    }
  }

  async function signOut() {
    if (submitting) return;
    submitting = 'logout';
    error = '';
    try {
      await api('/api/v1/logout', { method: 'POST' });
      phase = 'signin';
      status = null;
      apps = [];
      deployments = [];
      success = 'Signed out.';
      focusToken();
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'Logout failed';
    } finally {
      submitting = '';
    }
  }

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
      apps = Array.isArray(appsData.apps) ? appsData.apps : [];
      deployments = Array.isArray(deploymentsData.deployments) ? deploymentsData.deployments : [];
      for (const app of apps) {
        if (!(app.name in mapDomains)) mapDomains[app.name] = '';
      }
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) {
        phase = 'signin';
        focusToken();
      }
      error = cause instanceof Error ? cause.message : 'Daemon bridge unavailable';
    } finally {
      loading = false;
    }
  }

  async function submitMap(event, app) {
    event.preventDefault();
    const key = `map:${app}`;
    if (submitting) return;
    mapErrors[app] = '';
    success = '';
    submitting = key;
    try {
      await api('/api/v1/map-domain', {
        method: 'POST',
        body: JSON.stringify({ app, domain: mapDomains[app] ?? '' }),
      });
      success = `Domain mapped for ${app}.`;
      mapDomains[app] = '';
      await load();
    } catch (cause) {
      mapErrors[app] = cause instanceof Error ? cause.message : 'Domain mapping failed';
    } finally {
      submitting = '';
    }
  }

  async function submitDeploy(event) {
    event.preventDefault();
    if (submitting) return;
    deployError = '';
    success = '';
    deployedRoute = '';
    submitting = 'deploy';
    try {
      await api('/api/v1/deploy', {
        method: 'POST',
        body: JSON.stringify({ request: { ...deploy } }),
      });
      deployedRoute = deploy.domain;
      success = `Deploy submitted for ${deploy.app}. Refreshing node state…`;
      deployOpen = false;
      await load();
    } catch (cause) {
      deployError = cause instanceof Error ? cause.message : 'Deploy failed';
    } finally {
      submitting = '';
    }
  }

  function askRollback(app, deployment) {
    if (submitting) return;
    rollbackTarget = {
      app: app.name,
      deployment: deployment.id,
      expected_active_artifact: app.active?.artifact_hash ?? deployment.artifact_hash ?? '',
    };
  }

  async function confirmRollback() {
    if (!rollbackTarget || submitting) return;
    if (!rollbackTarget.expected_active_artifact) {
      error = 'The active artifact is unavailable; refresh before rolling back.';
      rollbackTarget = null;
      return;
    }
    const target = rollbackTarget;
    submitting = 'rollback';
    rollbackTarget = null;
    error = '';
    success = '';
    try {
      await api('/api/v1/rollback', { method: 'POST', body: JSON.stringify(target) });
      success = `Rollback submitted for ${target.app}. Refreshing node state…`;
      await load();
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'Rollback failed';
    } finally {
      submitting = '';
    }
  }

  function openDeploy(app = null) {
    deployOpen = true;
    if (app) {
      deploy.app = app.name;
      deploy.domain = app.domains?.[0] ?? '';
      deploy.engine_version = app.active?.engine_version ?? deploy.engine_version;
    }
  }

  function shortHash(value) {
    return value ? `${value.slice(0, 10)}…${value.slice(-6)}` : '—';
  }

  function focusToken() {
    setTimeout(() => tokenInput?.focus(), 0);
  }
  function focusMain() {
    setTimeout(() => document.querySelector('.live-shell h1')?.focus(), 0);
  }

  onMount(boot);
</script>

<svelte:head><title>Cygnus · Tenant Zero</title></svelte:head>

{#if phase === 'loading'}
  <div class="live-shell centered" aria-live="polite">Locating authenticated Tenant Zero…</div>
{:else if phase === 'locked'}
  <main class="live-shell centered auth-shell">
    <section class="auth-card" aria-labelledby="locked-title">
      <p class="eyebrow">TENANT ZERO · LOCKED</p>
      <h1 id="locked-title" tabindex="-1">Console locked</h1>
      <p class="lede">The live bridge is present, but operator authentication is not configured.</p>
      <p class="inline-error" role="alert">Set <code>CYGNUS_CONSOLE_BOOTSTRAP_TOKEN</code> and <code>CYGNUS_CONSOLE_SESSION_KEY</code> on the host, then reload.</p>
      <button class="primary" onclick={boot} disabled={loading}>{loading ? 'Checking…' : 'Retry configuration check'}</button>
    </section>
  </main>
{:else if phase === 'signin'}
  <main class="live-shell centered auth-shell">
    <section class="auth-card" aria-labelledby="signin-title">
      <p class="eyebrow">TENANT ZERO · OPERATOR ACCESS</p>
      <h1 id="signin-title" tabindex="-1">Sign in</h1>
      <p class="lede">Use the bootstrap token supplied by the host installer. It is exchanged for a short-lived operator session.</p>
      <form onsubmit={signIn} class="auth-form">
        <label for="bootstrap-token">Bootstrap token</label>
        <input id="bootstrap-token" bind:this={tokenInput} bind:value={token} type="password" autocomplete="current-password" autocapitalize="off" spellcheck="false" maxlength="1024" required />
        {#if authError}<p class="inline-error" role="alert">{authError}</p>{/if}
        <button class="primary" type="submit" disabled={submitting === 'signin' || !token}>{submitting === 'signin' ? 'Signing in…' : 'Open live console'}</button>
      </form>
      <p class="fine">Sessions expire after 12 hours. Sign out when leaving this machine.</p>
    </section>
  </main>
{:else}
  <main class="live-shell" aria-busy={loading}>
    <header class="mast">
      <div>
        <p class="eyebrow">TENANT ZERO · LIVE CONTROL PLANE</p>
        <h1 tabindex="-1">Cygnus</h1>
        <p class="lede">Daemon state and audited operations from the authenticated local operator session.</p>
      </div>
      <div class="mast-actions">
        <span class:down={!!error} class="bridge"><i></i>{error ? 'bridge unavailable' : 'daemon connected'}</span>
        <button onclick={load} disabled={loading || !!submitting}>{loading ? 'Refreshing…' : 'Refresh'}</button>
        <button onclick={signOut} disabled={!!submitting}>{submitting === 'logout' ? 'Signing out…' : 'Sign out'}</button>
      </div>
    </header>

    {#if success}
      <section class="success" role="status" aria-live="polite">{success}{#if deployedRoute} <a href={`http://${deployedRoute}`}>{deployedRoute}</a>{/if}</section>
    {/if}
    {#if error}
      <section class="fault" role="alert"><strong>Live state unavailable.</strong><span>{error}</span><button onclick={load} disabled={!!submitting}>Retry</button></section>
    {/if}

    {#if apps.length === 0 && !loading}
      <section class="panel onboarding" aria-labelledby="onboarding-title">
        <div class="panel-head"><div><p class="eyebrow">FIRST APP</p><h2 id="onboarding-title">Connect a host project</h2></div><span class="count num">1 / 1</span></div>
        <p class="onboarding-copy">Deploy your first app from an absolute host source path, then map its live route.</p>
        <button class="primary" onclick={() => openDeploy()} disabled={!!submitting}>Deploy first app</button>
      </section>
    {/if}

    {#if deployOpen}
      <section class="panel form-panel" aria-labelledby="deploy-title">
        <div class="panel-head"><div><p class="eyebrow">AUDITED DEPLOY</p><h2 id="deploy-title">Deploy from host</h2></div><button onclick={() => (deployOpen = false)} disabled={!!submitting}>Close</button></div>
        <form onsubmit={submitDeploy} class="deploy-form">
          <label>Source directory <input bind:value={deploy.source_dir} placeholder="/Users/me/project" required /></label>
          <label>App <input bind:value={deploy.app} placeholder="my-app" maxlength="64" required /></label>
          <label>Domain <input bind:value={deploy.domain} placeholder="my-app.localhost" maxlength="253" required /></label>
          <label>Engine version <input bind:value={deploy.engine_version} placeholder="bun-1.2.3" maxlength="128" required /></label>
          <label>Entry <input bind:value={deploy.entry} placeholder="index.ts" maxlength="4096" required /></label>
          <label>Artifact root <input bind:value={deploy.artifact_root} placeholder="/var/lib/cygnus/artifacts" required /></label>
          <label>Upstream socket <input bind:value={deploy.upstream} placeholder="/run/cygnus/my-app.sock" required /></label>
          {#if deployError}<p class="inline-error" role="alert">{deployError}</p>{/if}
          <div class="form-actions"><button class="primary" type="submit" disabled={!!submitting}>{submitting === 'deploy' ? 'Deploying…' : 'Deploy app'}</button></div>
        </form>
      </section>
    {:else if apps.length > 0}
      <button class="deploy-toggle" onclick={() => openDeploy()} disabled={!!submitting}>+ Deploy another app</button>
    {/if}

    {#if loading && !status}
      <section class="loading" aria-live="polite">Reading daemon state…</section>
    {:else}
      <section class="metrics" aria-label="Node summary">
        <article><span>Listener</span><strong class="num">{status?.listen ?? '—'}</strong></article>
        <article><span>Registered apps</span><strong class="num">{status?.app_count ?? apps.length}</strong></article>
        <article><span>Ready</span><strong class="num">{apps.filter((app) => app.lifecycle_state === 'ready').length}</strong></article>
        <article><span>Retained deploys</span><strong class="num">{deployments.length}</strong></article>
      </section>

      <section class="panel">
        <div class="panel-head"><div><p class="eyebrow">RUNTIME FLEET</p><h2>Apps</h2></div><span class="count num">{apps.length} shown</span></div>
        {#if apps.length === 0}<p class="empty">No apps are registered yet. Deploy from a host source path above.</p>{:else}
          <div class="table-wrap"><table><thead><tr><th>App</th><th>State</th><th>Routes</th><th>Policy</th><th>Active artifact</th><th>Map domain</th></tr></thead><tbody>
            {#each apps as app (app.name)}
              <tr>
                <td><strong>{app.name}</strong><small>{app.env_keys?.length ?? 0} env keys</small></td>
                <td><span class="state state-{app.lifecycle_state}"><i></i>{app.lifecycle_state}</span></td>
                <td>{#if app.domains?.length}{#each app.domains as domain}<code>{domain}</code>{/each}{:else}<span class="muted">unrouted</span>{/if}</td>
                <td><span>{app.pinned ? 'pinned' : `${Math.round((app.idle_ttl_ms ?? 0) / 60000)}m idle`}</span><small>{app.egress ?? '—'} egress</small></td>
                <td class="num">{shortHash(app.active?.artifact_hash)}</td>
                <td><form class="map-form" onsubmit={(event) => submitMap(event, app.name)}><input aria-label={`Domain for ${app.name}`} bind:value={mapDomains[app.name]} placeholder="app.example.com" maxlength="253" required /><button type="submit" disabled={!!submitting}>{submitting === `map:${app.name}` ? 'Saving…' : 'Map'}</button></form>{#if mapErrors[app.name]}<p class="inline-error" role="alert">{mapErrors[app.name]}</p>{/if}</td>
              </tr>
            {/each}
          </tbody></table></div>
        {/if}
      </section>

      <section class="panel">
        <div class="panel-head"><div><p class="eyebrow">IMMUTABLE HISTORY</p><h2>Deployments</h2></div><span class="count num">latest {deployments.length}</span></div>
        {#if deployments.length === 0}<p class="empty">No retained deployments.</p>{:else}
          <div class="deploy-grid">{#each deployments as deployment (deployment.id)}
            {@const deploymentApp = apps.find((app) => app.name === deployment.app)}
            <article><div><strong>{deployment.app}</strong><span class="state state-{deployment.status}"><i></i>{deployment.status}</span></div><code>{deployment.id}</code><p class="num">{shortHash(deployment.artifact_hash)} · {deployment.engine_version}</p>{#if deployment.error}<p class="deploy-error">{deployment.error}</p>{/if}{#if deploymentApp && deployment.artifact_hash}<button class="rollback" onclick={() => askRollback(deploymentApp, deployment)} disabled={!!submitting}>Roll back</button>{/if}</article>
          {/each}</div>
        {/if}
      </section>
    {/if}

    {#if rollbackTarget}
      <dialog open class="confirm" aria-labelledby="rollback-title" aria-describedby="rollback-copy"><div><p class="eyebrow">CONFIRM ROLLBACK</p><h2 id="rollback-title">Swap {rollbackTarget.app} to {rollbackTarget.deployment}?</h2><p id="rollback-copy">The active artifact will be checked before the retained deployment is promoted. No rebuild is started.</p></div><div class="form-actions"><button onclick={() => (rollbackTarget = null)}>Cancel</button><button class="danger" onclick={confirmRollback}>Confirm rollback</button></div></dialog>
    {/if}

    <footer>Authenticated as <code>local:operator</code>. Every mutation is sent to the daemon with the verified session actor.</footer>
  </main>
{/if}

<style>
  .live-shell { width: min(1380px, calc(100% - 48px)); margin: 0 auto; padding: 44px 0 72px; color: var(--ink-1); }
  .centered { min-height: 70vh; display: grid; place-items: center; }
  .auth-shell { width: min(100% - 32px, 560px); }
  .auth-card { width: 100%; border: 1px solid var(--line); border-radius: 14px; padding: clamp(24px, 5vw, 44px); background: color-mix(in srgb, var(--paper) 95%, transparent); }
  .eyebrow { margin: 0 0 8px; color: var(--blue); font: 600 10px/1.2 var(--mono); letter-spacing: .15em; }
  h1 { margin: 0; font-size: clamp(42px, 7vw, 88px); line-height: .9; letter-spacing: -.065em; }
  h2 { margin: 0; font-size: 28px; letter-spacing: -.035em; }
  .lede { margin: 18px 0 0; color: var(--ink-3); font-size: 14px; line-height: 1.55; overflow-wrap: anywhere; }
  .fine { color: var(--ink-4); font: 10px/1.6 var(--mono); }
  .mast { display: flex; justify-content: space-between; gap: 32px; align-items: flex-end; border-bottom: 1px solid var(--line); padding-bottom: 28px; }
  .mast-actions, .form-actions { display: flex; align-items: center; gap: 8px; flex-wrap: wrap; }
  button { border: 1px solid var(--line-2); background: var(--paper); color: var(--ink-1); border-radius: 8px; padding: 10px 14px; font: 600 11px var(--mono); cursor: pointer; }
  button:hover:not(:disabled) { border-color: var(--blue); }
  button:disabled { opacity: .5; cursor: wait; }
  .primary { background: var(--ink-1); color: var(--paper); border-color: var(--ink-1); }
  .danger { color: var(--red); border-color: color-mix(in srgb, var(--red) 45%, var(--line)); }
  .bridge, .state { display: inline-flex; align-items: center; gap: 7px; white-space: nowrap; font: 600 10px var(--mono); text-transform: uppercase; letter-spacing: .06em; }
  .bridge i, .state i { width: 6px; height: 6px; border-radius: 50%; background: var(--green); }
  .bridge.down i, .state-failed i { background: var(--red); }
  .state-cold i, .state-sealed i { background: var(--ink-4); }
  .state-building i, .state-booting i { background: var(--amber); }
  .success, .fault, .confirm { margin-top: 18px; border-radius: 10px; padding: 14px 18px; display: flex; gap: 12px; align-items: center; flex-wrap: wrap; line-height: 1.45; }
  .success { border: 1px solid color-mix(in srgb, var(--green) 35%, var(--line)); color: var(--green); }
  .success a { color: inherit; overflow-wrap: anywhere; }
  .fault { border: 1px solid color-mix(in srgb, var(--red) 35%, var(--line)); color: var(--red); }
  .fault span { color: var(--ink-3); overflow-wrap: anywhere; }
  .fault button { margin-left: auto; }
  .metrics { display: grid; grid-template-columns: repeat(4, 1fr); border: 1px solid var(--line); border-radius: 12px; overflow: hidden; margin: 34px 0 18px; }
  .metrics article { padding: 18px 20px; border-right: 1px solid var(--line); }
  .metrics article:last-child { border: 0; }
  .metrics span, small { display: block; color: var(--ink-4); font-size: 10px; }
  .metrics strong { display: block; margin-top: 8px; font-size: 19px; overflow-wrap: anywhere; }
  .panel { border: 1px solid var(--line); border-radius: 12px; margin-top: 18px; overflow: hidden; background: color-mix(in srgb, var(--paper) 94%, transparent); }
  .panel-head { display: flex; align-items: end; justify-content: space-between; gap: 18px; padding: 22px 24px; border-bottom: 1px solid var(--line); }
  .count { color: var(--ink-4); font-size: 10px; }
  .table-wrap { overflow-x: auto; }
  table { width: 100%; min-width: 860px; border-collapse: collapse; text-align: left; }
  th { color: var(--ink-4); font: 500 9px var(--mono); text-transform: uppercase; letter-spacing: .1em; }
  th, td { padding: 14px 18px; border-bottom: 1px solid var(--line); vertical-align: top; }
  tbody tr:last-child td { border-bottom: 0; }
  td { font-size: 12px; overflow-wrap: anywhere; }
  td strong { display: block; margin-bottom: 5px; }
  td code { display: block; margin-bottom: 4px; }
  code, .num { font-family: var(--mono); }
  code { color: var(--ink-2); font-size: 10px; overflow-wrap: anywhere; }
  .muted, .empty, footer, .loading { color: var(--ink-4); }
  .empty, .loading { padding: 28px 24px; margin: 0; }
  .deploy-grid { display: grid; grid-template-columns: repeat(3, 1fr); }
  .deploy-grid article { padding: 18px; border-right: 1px solid var(--line); border-bottom: 1px solid var(--line); overflow-wrap: anywhere; }
  .deploy-grid article > div { display: flex; justify-content: space-between; gap: 12px; margin-bottom: 16px; }
  .deploy-grid p { margin: 10px 0 0; color: var(--ink-3); font-size: 10px; }
  .deploy-grid .deploy-error, .inline-error { color: var(--red); }
  .rollback { margin-top: 16px; padding: 7px 10px; font-size: 10px; }
  footer { margin-top: 22px; font-size: 11px; line-height: 1.6; overflow-wrap: anywhere; }
  .auth-form, .deploy-form { display: grid; gap: 14px; margin-top: 26px; }
  label { display: grid; gap: 7px; color: var(--ink-2); font: 600 10px var(--mono); text-transform: uppercase; letter-spacing: .04em; }
  input { width: 100%; box-sizing: border-box; border: 1px solid var(--line-2); border-radius: 7px; background: var(--paper); color: var(--ink-1); padding: 11px 12px; font: 12px var(--mono); }
  input:focus-visible, button:focus-visible, a:focus-visible { outline: 2px solid var(--blue); outline-offset: 2px; }
  .deploy-form { grid-template-columns: repeat(2, 1fr); padding: 22px 24px 24px; }
  .deploy-form .inline-error, .deploy-form .form-actions { grid-column: 1 / -1; }
  .inline-error { margin: 3px 0 0; font-size: 11px; line-height: 1.5; overflow-wrap: anywhere; }
  .map-form { display: flex; gap: 6px; min-width: 220px; }
  .map-form input { min-width: 0; padding: 8px 9px; font-size: 10px; }
  .map-form button { padding: 8px 10px; }
  .onboarding { padding-bottom: 22px; }
  .onboarding-copy { margin: 20px 24px; color: var(--ink-3); font-size: 13px; line-height: 1.5; }
  .onboarding > .primary { margin-left: 24px; }
  .deploy-toggle { margin-top: 18px; }
  .confirm { justify-content: space-between; border: 1px solid color-mix(in srgb, var(--amber) 45%, var(--line)); background: color-mix(in srgb, var(--amber) 8%, var(--paper)); }
  .confirm p:not(.eyebrow) { max-width: 660px; margin: 8px 0 0; color: var(--ink-3); font-size: 12px; line-height: 1.5; }
  @media (max-width: 900px) { .live-shell { width: min(100% - 28px, 720px); padding-top: 28px; } .mast { align-items: flex-start; flex-direction: column; } .metrics { grid-template-columns: repeat(2, 1fr); } .metrics article:nth-child(2) { border-right: 0; } .deploy-grid { grid-template-columns: 1fr; } .deploy-grid article { border-right: 0; } }
  @media (max-width: 560px) { .mast-actions { width: 100%; justify-content: flex-start; } .metrics { grid-template-columns: 1fr; } .metrics article { border-right: 0; border-bottom: 1px solid var(--line); } .deploy-form { grid-template-columns: 1fr; padding: 18px; } .deploy-form .inline-error, .deploy-form .form-actions { grid-column: auto; } .panel-head { padding: 18px; } .onboarding-copy { margin-inline: 18px; } .onboarding > .primary { margin-left: 18px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { scroll-behavior: auto !important; transition-duration: .01ms !important; animation-duration: .01ms !important; animation-iteration-count: 1 !important; } }
</style>
