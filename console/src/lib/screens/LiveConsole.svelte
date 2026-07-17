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
  let submitting = $state('');
  let rollbackTarget = $state(null);
  let github = $state({ configured: false, app: null });
  let configuredRepos = $state([]);
  let installationRepos = $state([]);
  let jobs = $state([]);
  let githubOwner = $state('');
  let installationId = $state('');
  let githubError = $state('');
  let githubNotice = $state('');
  let repoErrors = $state({});
  let repoConfig = $state({});
  let mapDomains = $state({});
  let mapErrors = $state({});

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
        const callback = new URLSearchParams(window.location.search);
        const githubCallback = callback.get('github');
        const callbackInstallation = callback.get('installation_id');
        if (githubCallback === 'configured') githubNotice = 'GitHub App created. Install it, then return to choose repositories.';
        if (githubCallback === 'setup' && /^\d+$/.test(callbackInstallation ?? '') && Number(callbackInstallation) > 0) {
          installationId = callbackInstallation;
          githubNotice = 'GitHub App installed. Reading the selected repositories…';
          await loadInstallationRepositories();
        }
        if (githubCallback) window.history.replaceState({}, '', window.location.pathname);
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
      github = { configured: false, app: null };
      configuredRepos = [];
      installationRepos = [];
      jobs = [];
    } catch (cause) {
      error = cause instanceof Error ? cause.message : 'Logout failed';
    } finally {
      submitting = '';
    }
  }

  async function loadGithub() {
    githubError = '';
    try {
      const [statusData, repoData, jobData] = await Promise.all([
        api('/api/v1/github/status'),
        api('/api/v1/github/repositories?limit=50'),
        api('/api/v1/github/jobs?limit=50'),
      ]);
      github = { configured: statusData?.configured === true, app: statusData?.app ?? null };
      configuredRepos = Array.isArray(repoData?.repositories) ? repoData.repositories : [];
      jobs = Array.isArray(jobData?.jobs) ? jobData.jobs : [];
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) {
        phase = 'signin';
        focusToken();
      }
      githubError = cause instanceof Error ? cause.message : 'GitHub state unavailable';
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
      await loadGithub();
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

  async function connectGithub(event) {
    event.preventDefault();
    if (submitting) return;
    submitting = 'github-connect';
    githubError = '';
    githubNotice = '';
    try {
      const body = githubOwner.trim() ? JSON.stringify({ owner: githubOwner.trim() }) : JSON.stringify({});
      const result = await api('/api/v1/github/manifest', { method: 'POST', body });
      if (!result?.action || !result?.manifest) throw new Error('GitHub setup link was incomplete');
      const form = document.createElement('form');
      form.method = 'POST';
      form.action = result.action;
      form.hidden = true;
      const input = document.createElement('input');
      input.type = 'hidden';
      input.name = 'manifest';
      input.value = JSON.stringify(result.manifest);
      form.append(input);
      document.body.append(form);
      form.submit();
    } catch (cause) {
      githubError = cause instanceof Error ? cause.message : 'GitHub setup could not start';
    } finally {
      submitting = '';
    }
  }

  async function loadInstallationRepositories(event) {
    event.preventDefault();
    if (submitting) return;
    const id = installationId.trim();
    if (!/^\d+$/u.test(id) || Number(id) <= 0) {
      githubError = 'Enter the positive installation ID from GitHub.';
      return;
    }
    submitting = 'github-repositories';
    githubError = '';
    try {
      const result = await api(`/api/v1/github/installations/${encodeURIComponent(id)}/repositories`);
      installationRepos = Array.isArray(result?.repositories) ? result.repositories : [];
      githubNotice = installationRepos.length ? `${installationRepos.length} repositories available to configure.` : 'No repositories are available to this installation.';
      for (const repo of installationRepos) {
        if (!repoConfig[repo.repository_id]) repoConfig[repo.repository_id] = { app: repo.name, domain: '', engine_version: 'bun', entry: 'index.ts' };
      }
    } catch (cause) {
      githubError = cause instanceof Error ? cause.message : 'Repository discovery failed';
    } finally {
      submitting = '';
    }
  }

  async function configureRepository(event, repo) {
    event.preventDefault();
    if (submitting) return;
    const draft = repoConfig[repo.repository_id] ?? {};
    const key = `repo:${repo.repository_id}`;
    repoErrors[repo.repository_id] = '';
    submitting = key;
    try {
      await api('/api/v1/github/repositories', {
        method: 'POST',
        body: JSON.stringify({
          installation_id: repo.installation_id,
          repository_id: repo.repository_id,
          owner: repo.owner,
          name: repo.name,
          branch: repo.default_branch,
          app: draft.app ?? repo.name,
          domain: draft.domain ?? '',
          engine_version: draft.engine_version ?? 'bun',
          entry: draft.entry ?? 'index.ts',
        }),
      });
      githubNotice = `Mapped ${repo.full_name ?? `${repo.owner}/${repo.name}`} to Tenant Zero.`;
      await loadGithub();
    } catch (cause) {
      repoErrors[repo.repository_id] = cause instanceof Error ? cause.message : 'Repository configuration failed';
    } finally {
      submitting = '';
    }
  }

  async function retryJob(job) {
    if (submitting) return;
    submitting = `retry:${job.id}`;
    githubError = '';
    try {
      await api(`/api/v1/github/jobs/${encodeURIComponent(job.id)}/retry`, { method: 'POST' });
      githubNotice = `Retry queued for ${job.name}.`;
      await loadGithub();
    } catch (cause) {
      githubError = cause instanceof Error ? cause.message : 'Retry could not be queued';
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
      <section class="success" role="status" aria-live="polite">{success}</section>
    {/if}
    {#if error}
      <section class="fault" role="alert"><strong>Live state unavailable.</strong><span>{error}</span><button onclick={load} disabled={!!submitting}>Retry</button></section>
    {/if}

    <section class="panel github-panel" aria-labelledby="github-title">
      <div class="panel-head"><div><p class="eyebrow">GITHUB APP</p><h2 id="github-title">Connect Tenant Zero</h2></div><span class="count num">{github.configured ? 'CONNECTED' : 'STEP 1 / 3'}</span></div>
      {#if !github.configured}
        <p class="onboarding-copy">Create the private Cygnus GitHub App, install it on the repositories you want to deploy, then return here to map one branch.</p>
        <form class="github-start" onsubmit={connectGithub}>
          <label for="github-owner">Organization (optional)<input id="github-owner" bind:value={githubOwner} maxlength="39" placeholder="acme" autocomplete="organization" /></label>
          <p class="fine">Leave blank for a personal GitHub account. The browser receives only the public manifest; secrets stay in the daemon.</p>
          {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
          <button class="primary" type="submit" disabled={!!submitting}>{submitting === 'github-connect' ? 'Opening GitHub…' : 'Connect with GitHub'}</button>
        </form>
      {:else}
        <div class="github-connected">
          <div><span class="state state-ready"><i></i>app configured</span><strong>{github.app?.name ?? 'Cygnus Tenant Zero'}</strong><small>{github.app?.owner ?? 'GitHub owner not reported'} · {github.app?.html_url ?? 'private app'}</small></div>
          <p class="fine">Install the app from GitHub, then paste its installation ID to discover the repositories selected there.</p>
        </div>
        <form class="installation-form" onsubmit={loadInstallationRepositories}>
          <label for="installation-id">Installation ID<input id="installation-id" bind:value={installationId} inputmode="numeric" pattern="[0-9]+" required placeholder="12345678" /></label>
          <button class="primary" type="submit" disabled={!!submitting}>{submitting === 'github-repositories' ? 'Discovering…' : 'Discover repositories'}</button>
        </form>
        {#if installationRepos.length}
          <div class="repo-list" aria-label="Installation repositories">
            <div class="repo-list-head"><span>AVAILABLE REPOSITORIES</span><span>{installationRepos.length} shown</span></div>
            {#each installationRepos as repo (repo.repository_id)}
              {@const draft = repoConfig[repo.repository_id] ?? { app: repo.name, domain: '', engine_version: 'bun', entry: 'index.ts' }}
              <form class="repo-row" onsubmit={(event) => configureRepository(event, repo)}>
                <div class="repo-identity"><strong>{repo.full_name ?? `${repo.owner}/${repo.name}`}</strong><small>{repo.private ? 'private' : 'public'} · default {repo.default_branch}</small></div>
                <label>App<input value={draft.app} oninput={(event) => (repoConfig[repo.repository_id].app = event.currentTarget.value)} maxlength="64" required /></label>
                <label>Domain<input value={draft.domain} oninput={(event) => (repoConfig[repo.repository_id].domain = event.currentTarget.value)} maxlength="253" placeholder="app.example.com" required /></label>
                <label>Engine<input value={draft.engine_version} oninput={(event) => (repoConfig[repo.repository_id].engine_version = event.currentTarget.value)} maxlength="128" required /></label>
                <label>Entry<input value={draft.entry} oninput={(event) => (repoConfig[repo.repository_id].entry = event.currentTarget.value)} maxlength="4096" required /></label>
                <button class="primary" type="submit" disabled={!!submitting}>{submitting === `repo:${repo.repository_id}` ? 'Saving…' : 'Map repository'}</button>
                {#if repoErrors[repo.repository_id]}<p class="inline-error" role="alert">{repoErrors[repo.repository_id]}</p>{/if}
              </form>
            {/each}
          </div>
        {:else}
          <p class="empty">No installation repositories loaded yet. Discovery respects the selection made in GitHub.</p>
        {/if}
      {/if}
    </section>
    {#if githubNotice}<section class="success" role="status" aria-live="polite">{githubNotice}</section>{/if}
    {#if githubError && github.configured}<section class="fault" role="alert"><strong>GitHub state unavailable.</strong><span>{githubError}</span><button onclick={loadGithub} disabled={!!submitting}>Retry</button></section>{/if}

    {#if jobs.length || github.configured}
      <section class="panel jobs-panel" aria-labelledby="jobs-title">
        <div class="panel-head"><div><p class="eyebrow">WEBHOOK DELIVERY</p><h2 id="jobs-title">Deploy jobs</h2></div><span class="count num">{jobs.length} shown</span></div>
        {#if jobs.length === 0}<p class="empty">No webhook jobs yet. Push the default branch or open a pull request after mapping a repository.</p>{:else}
          <div class="table-wrap"><table><thead><tr><th>Repository</th><th>Environment</th><th>Revision</th><th>State</th><th>Attempts</th><th>Action</th></tr></thead><tbody>
            {#each jobs as job (job.id)}
              <tr>
                <td><strong>{job.owner}/{job.name}</strong><small>{job.kind}{#if job.pull_request} · PR #{job.pull_request}{/if}</small></td>
                <td>{job.environment}</td>
                <td><code>{shortHash(job.sha)}</code></td>
                <td><span class="state state-{job.status}"><i></i>{job.status}</span>{#if job.error}<p class="deploy-error">{job.error}</p>{/if}</td>
                <td class="num">{job.attempts}</td>
                <td>{#if ['failed', 'cancelled'].includes(job.status)}<button onclick={() => retryJob(job)} disabled={!!submitting}>{submitting === `retry:${job.id}` ? 'Retrying…' : 'Retry job'}</button>{:else}<span class="muted">—</span>{/if}</td>
              </tr>
            {/each}
          </tbody></table></div>
        {/if}
      </section>
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
        {#if apps.length === 0}<p class="empty">No runtime apps are registered yet. Configure a GitHub repository above to create the first deployment.</p>{:else}
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
  .github-panel { border-color: color-mix(in srgb, var(--blue) 28%, var(--line)); }
  .github-start, .installation-form { display: grid; gap: 14px; padding: 22px 24px 24px; }
  .github-start { max-width: 620px; }
  .github-start .primary { justify-self: start; }
  .github-connected { display: grid; gap: 8px; padding: 22px 24px 0; }
  .github-connected strong { display: block; margin-top: 9px; font-size: 18px; }
  .github-connected small { display: block; margin-top: 5px; color: var(--ink-4); overflow-wrap: anywhere; }
  .installation-form { grid-template-columns: minmax(180px, 320px) auto; align-items: end; }
  .repo-list { border-top: 1px solid var(--line); }
  .repo-list-head { display: flex; justify-content: space-between; gap: 16px; padding: 14px 24px; color: var(--ink-4); font: 600 9px var(--mono); letter-spacing: .1em; }
  .repo-row { display: grid; grid-template-columns: minmax(150px, 1.2fr) repeat(4, minmax(105px, 1fr)) auto; gap: 12px; align-items: end; padding: 18px 24px; border-top: 1px solid var(--line); }
  .repo-row label { min-width: 0; }
  .repo-row input { min-width: 0; }
  .repo-identity { min-width: 0; align-self: center; }
  .repo-identity strong, .repo-identity small { display: block; overflow-wrap: anywhere; }
  .repo-identity small { margin-top: 6px; color: var(--ink-4); font-size: 10px; }
  .repo-row .inline-error { grid-column: 1 / -1; }
  .jobs-panel .state { white-space: nowrap; }
  footer { margin-top: 22px; font-size: 11px; line-height: 1.6; overflow-wrap: anywhere; }
  .auth-form { display: grid; gap: 14px; margin-top: 26px; }
  label { display: grid; gap: 7px; color: var(--ink-2); font: 600 10px var(--mono); text-transform: uppercase; letter-spacing: .04em; }
  input { width: 100%; box-sizing: border-box; border: 1px solid var(--line-2); border-radius: 7px; background: var(--paper); color: var(--ink-1); padding: 11px 12px; font: 12px var(--mono); }
  input:focus-visible, button:focus-visible { outline: 2px solid var(--blue); outline-offset: 2px; }
  .inline-error { margin: 3px 0 0; font-size: 11px; line-height: 1.5; overflow-wrap: anywhere; }
  .map-form { display: flex; gap: 6px; min-width: 220px; }
  .map-form input { min-width: 0; padding: 8px 9px; font-size: 10px; }
  .map-form button { padding: 8px 10px; }
  .onboarding { padding-bottom: 22px; }
  .onboarding-copy { margin: 20px 24px; color: var(--ink-3); font-size: 13px; line-height: 1.5; }
  .confirm { justify-content: space-between; border: 1px solid color-mix(in srgb, var(--amber) 45%, var(--line)); background: color-mix(in srgb, var(--amber) 8%, var(--paper)); }
  .confirm p:not(.eyebrow) { max-width: 660px; margin: 8px 0 0; color: var(--ink-3); font-size: 12px; line-height: 1.5; }
  @media (max-width: 900px) { .live-shell { width: min(100% - 28px, 720px); padding-top: 28px; } .mast { align-items: flex-start; flex-direction: column; } .metrics { grid-template-columns: repeat(2, 1fr); } .metrics article:nth-child(2) { border-right: 0; } .deploy-grid { grid-template-columns: 1fr; } .deploy-grid article { border-right: 0; } .repo-row { grid-template-columns: repeat(2, minmax(0, 1fr)); } .repo-identity, .repo-row .primary, .repo-row .inline-error { grid-column: 1 / -1; } }
  @media (max-width: 560px) { .mast-actions { width: 100%; justify-content: flex-start; } .metrics { grid-template-columns: 1fr; } .metrics article { border-right: 0; border-bottom: 1px solid var(--line); } .installation-form { grid-template-columns: 1fr; } .repo-row { grid-template-columns: 1fr; padding-inline: 18px; } .repo-identity, .repo-row .primary, .repo-row .inline-error { grid-column: auto; } .panel-head { padding: 18px; } .onboarding-copy, .github-start, .installation-form, .github-connected { margin-inline: 0; padding-inline: 18px; } .repo-list-head { padding-inline: 18px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { scroll-behavior: auto !important; transition-duration: .01ms !important; animation-duration: .01ms !important; animation-iteration-count: 1 !important; } }
</style>
