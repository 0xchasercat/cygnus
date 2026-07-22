<script>
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { shortHash } from '../fmt.js';
  import Icon from '../components/Icon.svelte';
  import Identicon from '../components/Identicon.svelte';
  import { onMount } from 'svelte';

  // ——— domains ———
  let addOpen = $state(false);
  let newApp = $state('');
  let newDomain = $state('');
  let domainError = $state('');
  let domainBusy = $state(false);

  const allDomains = $derived(
    store.apps.flatMap((a) => (a.domains ?? []).map((d) => ({ app: a.name, domain: d })))
  );

  function certForDomain(domain) {
    const certs = store.node?.certificates ?? [];
    return certs.find((c) => c.domain === domain || (c.domain?.startsWith('*.') && domain.endsWith(c.domain.slice(1)))) ?? null;
  }

  async function submitDomain(e) {
    e.preventDefault();
    if (domainBusy || !newApp || !newDomain) return;
    if (!/^[a-z0-9.-]+$/i.test(newDomain)) {
      domainError = 'Domain may contain only letters, digits, dots, and hyphens.';
      return;
    }
    domainBusy = true;
    domainError = '';
    const r = await store.mapDomain(newApp, newDomain);
    domainBusy = false;
    if (!r.ok) {
      domainError = r.error ?? 'Domain mapping failed';
      return;
    }
    addOpen = false;
    newDomain = '';
  }

  // ——— gitops ———
  let githubOwner = $state('');
  let githubError = $state('');
  let githubBusy = $state(false);
  let repoQuery = $state('');
  let selectedRepoId = $state(null);
  let mapBusy = $state(false);
  let mapError = $state('');
  let mapDraft = $state({ app: '', domain: '', engine_version: '', entry: '' });

  const discoverableRepos = $derived(store.github.discoverable ?? []);
  const installationCount = $derived((store.github.installations ?? []).length);
  const appsDomain = $derived(store.node?.apps_domain ?? store.node?.apex_domain ?? '');
  const filteredRepos = $derived.by(() => {
    const q = repoQuery.trim().toLowerCase();
    const items = discoverableRepos;
    if (!q) return items;
    return items.filter((repo) => {
      const full = (repo.full_name ?? `${repo.owner}/${repo.name}`).toLowerCase();
      return full.includes(q) || String(repo.owner ?? '').toLowerCase().includes(q) || String(repo.name ?? '').toLowerCase().includes(q);
    });
  });
  const selectedRepo = $derived(
    selectedRepoId == null
      ? null
      : discoverableRepos.find((r) => r.repository_id === selectedRepoId) ?? null
  );

  // ——— dashboard domain + SSL ———
  let dashEditOpen = $state(false);
  let dashDomain = $state('');
  let dashApex = $state('');
  let dashError = $state('');
  let dashBusy = $state(false);
  let tlsBusy = $state(false);
  let acmeEmail = $state('');
  let acmeEmailOpen = $state(false);

  const dashboardDomain = $derived(store.node?.dashboard_domain ?? '');
  const apexDomain = $derived(store.node?.apex_domain ?? '');
  const sslMode = $derived(store.node?.ssl_mode ?? ''); // 'acme' | 'self_signed' | ''
  const sslAuto = $derived(sslMode === 'acme');
  const acmeEmailKnown = $derived(Boolean(store.node?.acme_email));

  const defaultEngine = $derived(
    store.node?.engines?.find((en) => en.default)?.version
    ?? store.node?.engines?.[0]?.version
    ?? ''
  );

  // Dashboard's own cert status, from the node's certificate list when present.
  const dashboardCert = $derived.by(() => {
    const certs = store.node?.certificates ?? [];
    if (!dashboardDomain) return null;
    return certs.find((c) => c.domain === dashboardDomain) ?? null;
  });
  const dashboardCertPill = $derived.by(() => {
    if (!dashboardDomain) return null;
    if (!dashboardCert) return sslAuto ? { cls: 'build', text: 'issuing' } : { cls: 'ghost', text: 'self-signed' };
    if (dashboardCert.kind === 'self_signed') return { cls: 'ghost', text: 'self-signed' };
    if (dashboardCert.ok) return { cls: 'live', text: 'trusted' };
    if (sslAuto) return { cls: 'build', text: 'issuing' };
    return { cls: 'ghost', text: 'self-signed' };
  });
  function openDashEdit() {
    dashDomain = dashboardDomain;
    // UI "Apps domain" is apex; fall back to apps_domain for older nodes.
    dashApex = apexDomain || store.node?.apps_domain || '';
    dashError = '';
    dashEditOpen = true;
  }

  async function saveDash(e) {
    e.preventDefault();
    if (dashBusy) return;
    const dom = dashDomain.trim().toLowerCase();
    const apex = dashApex.trim().toLowerCase();
    if (dom && !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(dom)) {
      dashError = 'Enter a domain like dashboard.example.com.';
      return;
    }
    if (apex && !/^[a-z0-9.-]+\.[a-z]{2,}$/i.test(apex)) {
      dashError = 'Apps domain looks off — check the spelling.';
      return;
    }
    dashBusy = true;
    dashError = '';
    const r = await store.setDashboardDomain(dom, apex);
    dashBusy = false;
    if (!r.ok) {
      dashError = r.error ?? 'Could not update dashboard domain';
      return;
    }
    dashEditOpen = false;
  }

  async function toggleDashboardTls() {
    if (tlsBusy || !sslMode) return;
    // Enabling ACME needs a contact email. Prefer the stored one; otherwise
    // open the inline field so we don't silently leave self-signed fallback.
    if (!sslAuto && !acmeEmailKnown && !acmeEmailOpen) {
      acmeEmailOpen = true;
      dashError = 'Enter a contact email for Let\'s Encrypt, then enable again.';
      return;
    }
    tlsBusy = true;
    dashError = '';
    const next = sslAuto ? 'self_signed' : 'acme';
    const email = !sslAuto ? (acmeEmail.trim() || undefined) : undefined;
    if (next === 'acme' && !acmeEmailKnown && !email) {
      tlsBusy = false;
      acmeEmailOpen = true;
      dashError = 'Let\'s Encrypt needs a contact email.';
      return;
    }
    const r = await store.setDashboardTls(next, email);
    tlsBusy = false;
    if (!r.ok) {
      dashError = r.error ?? 'Could not change dashboard TLS';
      return;
    }
    acmeEmailOpen = false;
  }

  async function connectGithub(e) {
    e.preventDefault();
    if (githubBusy) return;
    githubBusy = true;
    githubError = '';
    const r = await store.githubManifest(githubOwner.trim() || undefined);
    githubBusy = false;
    if (!r.ok) githubError = r.error ?? 'GitHub setup could not start';
  }

  function defaultDraftFor(repo) {
    return {
      app: repo.name,
      domain: appsDomain ? `${repo.name}.${appsDomain}` : '',
      engine_version: defaultEngine,
      entry: '',
    };
  }

  function selectRepo(repo) {
    selectedRepoId = repo.repository_id;
    mapError = '';
    mapDraft = defaultDraftFor(repo);
  }

  function clearSelectedRepo() {
    selectedRepoId = null;
    mapError = '';
  }

  async function refreshDiscoverable() {
    if (githubBusy) return;
    githubBusy = true;
    githubError = '';
    const r = await store.discoverRepositories();
    githubBusy = false;
    if (!r.ok) githubError = r.error ?? 'Repository discovery failed';
    // Keep selection only if the repo is still present.
    if (selectedRepoId != null && !(r.repositories ?? []).some((repo) => repo.repository_id === selectedRepoId)) {
      clearSelectedRepo();
    }
  }

  async function configureSelected(e) {
    e.preventDefault();
    const repo = selectedRepo;
    if (!repo || mapBusy) return;
    mapBusy = true;
    mapError = '';
    const r = await store.configureRepository({
      installation_id: repo.installation_id,
      repository_id: repo.repository_id,
      owner: repo.owner,
      name: repo.name,
      branch: repo.default_branch,
      app: mapDraft.app || repo.name,
      domain: mapDraft.domain || '',
      engine_version: mapDraft.engine_version || defaultEngine,
      entry: (mapDraft.entry ?? '').trim() || undefined,
    });
    mapBusy = false;
    if (!r.ok) {
      mapError = r.error ?? 'Repository configuration failed';
      return;
    }
    clearSelectedRepo();
    await store.refreshGithub();
  }

  async function retryJob(job) {
    await store.retryJob(job.id);
  }

  async function signOut() {
    await store.signOut();
  }

  // ——— password change ———
  let pwOpen = $state(false);
  let pwEmail = $state('');
  let pwCurrent = $state('');
  let pwNew = $state('');
  let pwConfirm = $state('');
  let pwError = $state('');
  let pwBusy = $state(false);
  let pwDone = $state(false);

  function openPasswordForm() {
    pwOpen = true;
    pwEmail = '';
    pwCurrent = '';
    pwNew = '';
    pwConfirm = '';
    pwError = '';
    pwDone = false;
  }

  async function submitPasswordChange(e) {
    e.preventDefault();
    if (pwBusy) return;
    if (pwNew.length < 12) {
      pwError = 'New password must be at least 12 characters.';
      return;
    }
    if (pwNew !== pwConfirm) {
      pwError = 'New password and confirmation do not match.';
      return;
    }
    pwBusy = true;
    pwError = '';
    const r = await store.changePassword({
      email: pwEmail.trim(),
      currentPassword: pwCurrent,
      newPassword: pwNew,
    });
    pwBusy = false;
    if (!r.ok) {
      pwError = r.error ?? 'Could not change password';
      return;
    }
    pwOpen = false;
    pwDone = true;
    pwCurrent = '';
    pwNew = '';
    pwConfirm = '';
  }

  const recentJobs = $derived(store.github.jobs.slice(0, 5));

  // Auto-load discoverable repos — never ask for an installation ID.
  onMount(() => {
    if (store.github.configured && !discoverableRepos.length) refreshDiscoverable();
  });
</script>

<div class="page screen-enter">
  <div class="head">
    <h1>Settings</h1>
  </div>

  {#if store.notice}
    <div class="notice" role="status"><Icon name="check" size={13} /> {store.notice}</div>
  {/if}

  <div class="grid">
    <div class="col">
      <!-- domains -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Domains</span>
          <button class="btn sm" onclick={() => (addOpen = !addOpen)}><Icon name="plus" size={12} />{addOpen ? 'Cancel' : 'Add domain'}</button>
        </div>
        {#if addOpen}
          <form class="addform" onsubmit={submitDomain}>
            <label>App
              <select bind:value={newApp} required>
                <option value="" disabled>Select an app</option>
                {#each store.apps as a (a.name)}<option value={a.name}>{a.name}</option>{/each}
              </select>
            </label>
            <label>Domain
              <input bind:value={newDomain} placeholder="app.example.com" maxlength="253" required />
            </label>
            <button class="btn cobalt sm" type="submit" disabled={domainBusy || !newApp || !newDomain}>
              {domainBusy ? 'Mapping…' : 'Map domain'}
            </button>
            {#if domainError}<p class="inline-error" role="alert">{domainError}</p>{/if}
          </form>
        {/if}
        <div class="rows pad0">
          {#each allDomains as d (d.app + d.domain)}
            {@const cert = certForDomain(d.domain)}
            <div class="row">
              <span class="glyph"><Icon name="globe" size={14} /></span>
              <div class="tk">
                <span class="mname num">{d.domain}</span>
                <span class="tmeta num">{d.app}</span>
              </div>
              <span class="grow"></span>
              <span class="led {cert ? (cert.ok ? 'live' : 'build') : 'cold'}"></span>
              <span class="certstate num">{cert ? (cert.ok ? 'ok' : 'pending') : '—'}</span>
            </div>
          {/each}
          {#if !allDomains.length}<div class="empty mono">no domains mapped</div>{/if}
        </div>
      </section>

      <!-- gitops -->
      <section class="card">
        <div class="cardhead"><span class="label">GitOps</span></div>
        {#if !store.github.configured}
          <div class="pad">
            <p class="git-copy">Deploy on push from your GitHub repositories.</p>
            <form class="github-start" onsubmit={connectGithub}>
              <label>Organization (optional)
                <input bind:value={githubOwner} maxlength="39" placeholder="acme" autocomplete="organization" />
              </label>
              <button class="btn cobalt sm" type="submit" disabled={githubBusy}>
                {githubBusy ? 'Opening GitHub…' : 'Connect GitHub'}
              </button>
              {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
            </form>
          </div>
        {:else}
          <div class="rows pad0">
            <div class="row">
              <span class="glyph"><Icon name="branch" size={14} /></span>
              <div class="tk">
                <span class="mname">{store.github.app?.name ?? 'Cygnus GitHub App'}</span>
                <span class="tmeta num">{store.github.app?.owner ?? '—'} · {store.github.repositories.length} mapped · {installationCount} install{installationCount === 1 ? '' : 's'}</span>
              </div>
              <span class="grow"></span>
              <span class="led live"></span>
            </div>
            {#each store.github.repositories as r (r.repository_id)}
              <div class="row subrow">
                <Identicon name={r.name} size={20} />
                <div class="tk">
                  <span class="mname num">{r.full_name ?? `${r.owner}/${r.name}`}</span>
                  <span class="tmeta num">{r.app} · {r.branch ?? r.default_branch ?? 'main'}</span>
                </div>
              </div>
            {/each}
          </div>
          <div class="install-block">
            <div class="install-cta">
              {#if store.github.app?.html_url}
                <a class="btn cobalt sm" href="{store.github.app.html_url}/installations/new">
                  {installationCount ? 'Manage GitHub access' : 'Install App on GitHub'} <Icon name="arrowR" size={12} />
                </a>
              {/if}
              <button class="btn sm" type="button" onclick={refreshDiscoverable} disabled={githubBusy}>
                {githubBusy ? 'Refreshing…' : 'Refresh'}
              </button>
              <p class="install-desc">
                Search your accessible repositories, pick one, then configure the mapping.
              </p>
              {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
            </div>

            {#if selectedRepo}
              <form class="map-panel" onsubmit={configureSelected}>
                <div class="map-head">
                  <div class="repo-identity">
                    <strong>{selectedRepo.full_name ?? `${selectedRepo.owner}/${selectedRepo.name}`}</strong>
                    <small>{selectedRepo.private ? 'private' : 'public'} · default {selectedRepo.default_branch}</small>
                  </div>
                  <button class="btn sm" type="button" onclick={clearSelectedRepo}>Back</button>
                </div>
                <div class="repo-fields">
                  <label>App<input bind:value={mapDraft.app} maxlength="64" required /></label>
                  <label>Domain<input bind:value={mapDraft.domain} maxlength="253" placeholder="app.example.com" required /></label>
                  <label>Engine<input bind:value={mapDraft.engine_version} maxlength="128" required /></label>
                  <label>Entry <span class="optional">(optional)</span><input bind:value={mapDraft.entry} maxlength="128" placeholder="auto" /></label>
                </div>
                {#if mapError}<p class="inline-error" role="alert">{mapError}</p>{/if}
                <div class="map-actions">
                  <button class="btn cobalt sm" type="submit" disabled={mapBusy}>
                    {mapBusy ? 'Mapping…' : 'Map repository'}
                  </button>
                </div>
              </form>
            {:else if discoverableRepos.length}
              <div class="repo-search">
                <input
                  type="search"
                  bind:value={repoQuery}
                  placeholder="Search repositories…"
                  aria-label="Search repositories"
                />
                <span class="repo-search-count num">{filteredRepos.length} of {discoverableRepos.length}</span>
              </div>
              <div class="repo-pick-list">
                {#each filteredRepos as repo (repo.repository_id)}
                  {@const alreadyMapped = store.github.repositories.some((r) => r.repository_id === repo.repository_id)}
                  <button
                    type="button"
                    class="repo-pick"
                    class:mapped={alreadyMapped}
                    onclick={() => selectRepo(repo)}
                  >
                    <div class="repo-identity">
                      <strong>{repo.full_name ?? `${repo.owner}/${repo.name}`}</strong>
                      <small>{repo.private ? 'private' : 'public'} · {repo.default_branch}{#if alreadyMapped} · already mapped{/if}</small>
                    </div>
                    <Icon name="arrowR" size={12} />
                  </button>
                {:else}
                  <div class="empty mono">no repositories match “{repoQuery}”</div>
                {/each}
              </div>
            {:else if githubBusy}
              <div class="empty mono">discovering repositories…</div>
            {:else}
              <div class="empty mono">no repositories yet — install the app, then refresh</div>
            {/if}
          </div>
        {/if}
        {#if recentJobs.length}
          <div class="jobs">
            <div class="jobs-head"><span class="label">Recent jobs</span></div>
            <div class="rows pad0">
              {#each recentJobs as job (job.id)}
                <div class="row">
                  <span class="led {job.status === 'failed' || job.status === 'cancelled' ? 'fail' : job.status === 'running' ? 'build' : 'live'}"></span>
                  <div class="tk">
                    <span class="mname num">{job.owner}/{job.name}</span>
                    <span class="tmeta num">{job.kind}{#if job.pull_request} · PR #{job.pull_request}{/if} · {shortHash(job.sha).slice(0, 7)}</span>
                  </div>
                  <span class="grow"></span>
                  {#if job.status === 'failed' || job.status === 'cancelled'}
                    <button class="btn sm" onclick={() => retryJob(job)}>Retry</button>
                  {/if}
                </div>
              {/each}
            </div>
          </div>
        {/if}
        <div class="foot num">builds run server-side · lifecycle scripts disabled by default</div>
      </section>
    </div>

    <div class="col">
      <!-- dashboard domain + SSL -->
      <section class="card">
        <div class="cardhead">
          <span class="label">Dashboard domain</span>
          {#if dashboardCertPill}
            <span class="pill {dashboardCertPill.cls}">{dashboardCertPill.text}</span>
          {/if}
        </div>
        <div class="pad">
          <div class="dash-row">
            <div class="dash-domain">
              <span class="mname num">{dashboardDomain || 'unset'}</span>
              <span class="tmeta num">
                {#if dashboardDomain}
                  {#if apexDomain}apps at *.{apexDomain}{/if}
                {:else}
                  reachable by IP
                {/if}
              </span>
            </div>
            <button class="btn sm" onclick={openDashEdit}>{dashEditOpen ? 'Cancel' : 'Edit'}</button>
          </div>

          {#if dashEditOpen}
            <form class="dash-form" onsubmit={saveDash}>
              <label>Dashboard domain
                <input bind:value={dashDomain} maxlength="253" placeholder="dashboard.example.com" />
              </label>
              <label>Apps domain
                <input bind:value={dashApex} maxlength="253" placeholder="example.com" />
              </label>
              <button class="btn cobalt sm" type="submit" disabled={dashBusy}>
                {dashBusy ? 'Saving…' : 'Save domain'}
              </button>
              {#if dashError}<p class="inline-error" role="alert">{dashError}</p>{/if}
            </form>
          {/if}

          <div class="hairline-h dash-hl"></div>

          <div class="tls-row">
            <div class="tls-meta">
              <span class="tls-title">Automatic HTTPS</span>
              <span class="tmeta num">
                {#if !sslMode}not configured{:else if sslAuto}Let's Encrypt · trusted certificate{:else}self-signed · browsers will warn{/if}
              </span>
            </div>
            {#if sslMode}
              <button
                type="button"
                class="toggle {sslAuto ? 'on' : ''}"
                onclick={toggleDashboardTls}
                aria-pressed={sslAuto}
                aria-label={sslAuto ? 'Disable automatic HTTPS' : 'Enable automatic HTTPS'}
                disabled={tlsBusy}
              >
                <span class="track"><span class="thumb"></span></span>
              </button>
            {/if}
          </div>
          {#if acmeEmailOpen && !sslAuto}
            <label class="dash-form" style="margin-top:10px">
              Let's Encrypt contact email
              <input
                bind:value={acmeEmail}
                type="email"
                autocomplete="email"
                placeholder="ops@example.com"
                maxlength="254"
              />
            </label>
          {/if}
          {#if dashError && !dashEditOpen}<p class="inline-error" role="alert">{dashError}</p>{/if}
          {#if sslMode}
            <p class="dash-note mono">
              {#if sslAuto}
                DNS A record must point at this node, and ports 80/443 must be reachable from the public internet. Until ACME succeeds, a self-signed cert (CN "Cygnus self-signed fallback") is served.
              {:else}
                Switch to automatic HTTPS to issue a trusted certificate from Let's Encrypt. You will need a contact email.
              {/if}
            </p>
          {/if}
        </div>
      </section>

      <!-- access -->
      <section class="card">
        <div class="cardhead"><span class="label">Access</span></div>
        <div class="pad">
          <div class="access-row">
            <span class="mname">Admin password</span>
            {#if !pwOpen}
              <button class="btn sm" onclick={openPasswordForm}>Change password</button>
            {/if}
          </div>
          {#if pwDone}
            <p class="pwdone" role="status"><Icon name="check" size={12} /> Password changed.</p>
          {/if}
          {#if pwOpen}
            <form class="pwform" onsubmit={submitPasswordChange}>
              <label>Email<input type="email" bind:value={pwEmail} autocomplete="email" required /></label>
              <label>Current password<input type="password" bind:value={pwCurrent} autocomplete="current-password" required /></label>
              <label>New password<input type="password" bind:value={pwNew} autocomplete="new-password" minlength="12" required /></label>
              <label>Confirm new password<input type="password" bind:value={pwConfirm} autocomplete="new-password" minlength="12" required /></label>
              {#if pwError}<p class="pwerr" role="alert">{pwError}</p>{/if}
              <div class="pwactions">
                <button class="btn sm" type="button" onclick={() => (pwOpen = false)} disabled={pwBusy}>Cancel</button>
                <button class="btn cobalt sm" type="submit" disabled={pwBusy}>{pwBusy ? 'Changing…' : 'Change password'}</button>
              </div>
            </form>
          {/if}
          <div class="access-row">
            <span class="mname">Host break-glass</span>
            <span class="tmeta num">cygnus --admin-socket ~/.cygnus/run/admin.sock status</span>
          </div>
          {#if store.mode === 'live'}
            <button class="btn sm danger" onclick={signOut}>Sign out</button>
          {/if}
        </div>
      </section>

      <!-- domains mirror placeholder to keep the column balanced -->
      <section class="card">
        <div class="cardhead"><span class="label">Node</span></div>
        <div class="rows pad0">
          <div class="row">
            <span class="glyph"><Icon name="node" size={14} /></span>
            <div class="tk">
              <span class="mname num">{store.node?.apps_domain ?? store.node?.apex_domain ?? 'cygnus'}</span>
              <span class="tmeta num">{store.node?.version ?? '—'} · {store.node?.listen ?? '—'}</span>
            </div>
            <span class="grow"></span>
          </div>
          {#if store.node?.engines?.length}
            {#each store.node.engines as e (e.version)}
              <div class="row subrow">
                <span class="glyph"><Icon name="terminal" size={13} /></span>
                <div class="tk">
                  <span class="mname num">{e.version}</span>
                  <span class="tmeta num">{e.apps ?? 0} apps{#if e.default} · default{/if}</span>
                </div>
              </div>
            {/each}
          {/if}
        </div>
      </section>
    </div>
  </div>
</div>

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 26px 44px 0;
  }
  .head { margin-bottom: 18px; }
  h1 {
    font-size: 23px;
    font-weight: 650;
    letter-spacing: -0.02em;
  }

  .grid {
    display: grid;
    grid-template-columns: repeat(2, 1fr);
    gap: 18px;
    align-items: start;
  }
  .col { display: flex; flex-direction: column; gap: 18px; }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
  }
  .pad0 { padding: 0 10px 8px; }
  .pad { padding: 2px 18px 16px; }

  .row {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 11px 10px;
  }
  .row + .row { border-top: 1px solid var(--line-2); }
  .mname { font-size: 13px; font-weight: 600; }
  .grow { flex: 1; }
  .glyph {
    width: 28px;
    height: 28px;
    border-radius: 9px;
    background: var(--surface-3);
    color: var(--ink-2);
    display: grid;
    place-items: center;
    flex: none;
  }
  .tk { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
  .tmeta { font-size: 11px; color: var(--ink-3); }
  .foot {
    padding: 10px 18px 14px;
    font-size: 10.5px;
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
    font-family: var(--mono);
  }

  .subrow { padding-left: 18px; }
  .row.subrow + .row.subrow { border-top: 1px solid var(--line-2); }

  .notice {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-bottom: 16px;
    padding: 10px 14px;
    border: 1px solid color-mix(in srgb, var(--live) 35%, var(--line));
    border-radius: 11px;
    background: var(--live-soft);
    color: #087a45;
    font-size: 12px;
  }

  .addform {
    display: grid;
    grid-template-columns: minmax(120px, 180px) minmax(160px, 1fr) auto;
    gap: 10px;
    align-items: end;
    padding: 14px 18px 16px;
    border-bottom: 1px solid var(--line-2);
  }
  .addform label {
    display: grid;
    gap: 5px;
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .addform input, .addform select {
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 10px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .addform .inline-error { grid-column: 1 / -1; }

  .git-copy { margin: 0 0 12px; font-size: 13px; color: var(--ink-2); line-height: 1.5; }
  .github-start { display: grid; gap: 10px; max-width: 360px; }
  .github-start label {
    display: grid; gap: 5px;
    font-family: var(--mono); font-size: 10px; letter-spacing: 0.08em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .github-start input {
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 9px 10px; font-family: var(--mono); font-size: 12px;
  }

  .certstate { font-size: 10.5px; color: var(--ink-3); width: 56px; text-align: right; }

  .install-cta { margin-bottom: 14px; display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  .install-desc { margin: 0; flex: 1 1 100%; font-size: 12px; color: var(--ink-2); line-height: 1.5; }
  .optional { font-size: 9px; opacity: 0.65; text-transform: lowercase; font-weight: normal; letter-spacing: 0; }

  .install-block { border-top: 1px solid var(--line-2); padding: 14px 18px; min-width: 0; }

  .repo-search {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 10px;
    align-items: center;
    margin-bottom: 10px;
  }
  .repo-search input {
    width: 100%;
    min-width: 0;
    box-sizing: border-box;
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 10px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .repo-search-count { font-size: 11px; color: var(--ink-3); white-space: nowrap; }

  .repo-pick-list {
    display: flex;
    flex-direction: column;
    gap: 6px;
    max-height: 360px;
    overflow-y: auto;
    min-width: 0;
  }
  .repo-pick {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    width: 100%;
    text-align: left;
    padding: 11px 12px;
    border: 1px solid var(--line-2);
    border-radius: 10px;
    background: var(--surface);
    color: var(--ink);
    cursor: pointer;
    min-width: 0;
  }
  .repo-pick:hover { border-color: var(--cobalt); background: var(--cobalt-ghost); }
  .repo-pick.mapped { opacity: 0.72; }
  .repo-identity { min-width: 0; flex: 1; }
  .repo-identity strong {
    display: block;
    font-size: 12.5px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .repo-identity small { display: block; margin-top: 4px; color: var(--ink-4); font-size: 10px; }

  .map-panel {
    display: flex;
    flex-direction: column;
    gap: 12px;
    padding: 12px;
    border: 1px solid var(--line-2);
    border-radius: 10px;
    min-width: 0;
  }
  .map-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    min-width: 0;
  }
  .repo-fields {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 10px;
    min-width: 0;
  }
  .repo-fields label {
    display: grid; gap: 4px; min-width: 0;
    font-family: var(--mono); font-size: 9.5px; letter-spacing: 0.06em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .repo-fields input {
    width: 100%;
    min-width: 0;
    box-sizing: border-box;
    border: 1px solid var(--line-strong);
    border-radius: 7px; background: var(--surface); color: var(--ink);
    padding: 7px 9px; font-family: var(--mono); font-size: 11.5px;
  }
  .map-actions { display: flex; justify-content: flex-end; }

  .jobs { border-top: 1px solid var(--line-2); }
  .jobs-head { padding: 12px 18px 4px; }

  .access-row {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 14px;
    padding: 10px 0;
  }
  .access-row + .access-row { border-top: 1px solid var(--line-2); }
  .access-row .tmeta { flex: 1; text-align: right; overflow-wrap: anywhere; }
  .pwform {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
    padding: 12px 0 4px;
  }
  .pwform label {
    display: grid;
    gap: 5px;
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .pwform input {
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 10px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .pwerr { grid-column: 1 / -1; color: var(--red); font-size: 11.5px; margin: 0; }
  .pwactions { grid-column: 1 / -1; display: flex; justify-content: flex-end; gap: 8px; margin-top: 2px; }
  .pwdone {
    display: flex;
    align-items: center;
    gap: 6px;
    margin: 8px 0 0;
    font-size: 11.5px;
    color: var(--live);
  }

  /* dashboard domain + SSL card */
  .dash-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    padding: 4px 0 14px;
  }
  .dash-domain { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
  .dash-domain .mname { font-size: 13px; font-weight: 600; overflow-wrap: anywhere; }
  .dash-form {
    display: grid;
    grid-template-columns: 1fr 1fr auto;
    gap: 10px;
    align-items: end;
    padding: 12px 0 14px;
    border-top: 1px solid var(--line-2);
    margin-top: -4px;
  }
  .dash-form label {
    display: grid;
    gap: 5px;
    font-family: var(--mono);
    font-size: 10px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
  }
  .dash-form input {
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 10px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .dash-form .inline-error { grid-column: 1 / -1; }
  .dash-hl { margin: 4px 0 0; }
  .tls-row {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 14px;
    padding: 12px 0 4px;
  }
  .tls-meta { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
  .tls-title { font-size: 13px; font-weight: 600; }
  .dash-note {
    margin: 8px 0 0;
    font-size: 10.5px;
    line-height: 1.6;
    color: var(--ink-4);
  }
  .toggle {
    flex: none;
    width: 38px;
    height: 22px;
    padding: 0;
    border: none;
    background: transparent;
    cursor: pointer;
  }
  .toggle:disabled { cursor: not-allowed; opacity: 0.5; }
  .track {
    display: block;
    width: 38px;
    height: 22px;
    border-radius: 22px;
    background: var(--line-strong);
    position: relative;
    transition: background 0.18s ease;
  }
  .toggle.on .track { background: var(--cobalt); }
  .thumb {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: #fff;
    box-shadow: 0 1px 2px rgba(13, 18, 28, 0.2);
    transition: transform 0.18s cubic-bezier(0.22, 1, 0.36, 1);
  }
  .toggle.on .thumb { transform: translateX(16px); }

  .empty {
    padding: 28px 18px;
    text-align: center;
    font-size: 11px;
    color: var(--ink-4);
    letter-spacing: 0.06em;
  }
  .inline-error { color: var(--red); font-size: 11px; margin: 2px 0 0; }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
    .addform, .dash-form, .repo-search { grid-template-columns: 1fr; }
    .repo-fields { grid-template-columns: 1fr; }
  }
</style>
