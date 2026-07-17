<script>
  import { store } from '../live.svelte.js';
  import { ui } from '../stores.svelte.js';
  import { relativeTime } from '../time.js';
  import { shortHash } from '../fmt.js';
  import Icon from '../components/Icon.svelte';
  import Identicon from '../components/Identicon.svelte';

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
  let installationId = $state('');
  let installationRepos = $state([]);
  let repoConfig = $state({});
  let repoErrors = $state({});

  async function connectGithub(e) {
    e.preventDefault();
    if (githubBusy) return;
    githubBusy = true;
    githubError = '';
    const r = await store.githubManifest(githubOwner.trim() || undefined);
    githubBusy = false;
    if (!r.ok) githubError = r.error ?? 'GitHub setup could not start';
  }

  async function discoverRepos(e) {
    e.preventDefault();
    const id = installationId.trim();
    if (!/^\d+$/u.test(id) || Number(id) <= 0) {
      githubError = 'Enter the positive installation ID from GitHub.';
      return;
    }
    githubBusy = true;
    githubError = '';
    const r = await store.listInstallationRepositories(id);
    githubBusy = false;
    installationRepos = r.repositories;
    if (!r.ok) githubError = r.error ?? 'Repository discovery failed';
    for (const repo of installationRepos) {
      if (!repoConfig[repo.repository_id]) {
        repoConfig[repo.repository_id] = { app: repo.name, domain: '', engine_version: 'bun', entry: 'index.ts' };
      }
    }
  }

  async function configureRepo(e, repo) {
    e.preventDefault();
    const draft = repoConfig[repo.repository_id] ?? {};
    const r = await store.configureRepository({
      installation_id: repo.installation_id,
      repository_id: repo.repository_id,
      owner: repo.owner,
      name: repo.name,
      branch: repo.default_branch,
      app: draft.app ?? repo.name,
      domain: draft.domain ?? '',
      engine_version: draft.engine_version ?? 'bun',
      entry: draft.entry ?? 'index.ts',
    });
    if (!r.ok) repoErrors[repo.repository_id] = r.error ?? 'Repository configuration failed';
  }

  async function retryJob(job) {
    await store.retryJob(job.id);
  }

  async function signOut() {
    await store.signOut();
  }

  const recentJobs = $derived(store.github.jobs.slice(0, 5));
</script>

<div class="page screen-enter">
  <div class="head">
    <h1>Settings</h1>
    <p class="sub">One node, one binary. Everything else is a row in SQLite.</p>
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
                <span class="tmeta num">{store.github.app?.owner ?? '—'} · {store.github.repositories.length} repo{store.github.repositories.length === 1 ? '' : 's'}</span>
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
            <form class="install-form" onsubmit={discoverRepos}>
              <label>Installation ID
                <input bind:value={installationId} inputmode="numeric" pattern="[0-9]+" placeholder="12345678" />
              </label>
              <button class="btn sm" type="submit" disabled={githubBusy}>{githubBusy ? 'Discovering…' : 'Discover repositories'}</button>
            </form>
            {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
            {#if installationRepos.length}
              <div class="repo-list">
                {#each installationRepos as repo (repo.repository_id)}
                  {@const draft = repoConfig[repo.repository_id] ?? { app: repo.name, domain: '', engine_version: 'bun', entry: 'index.ts' }}
                  <form class="repo-row" onsubmit={(e) => configureRepo(e, repo)}>
                    <div class="repo-identity"><strong>{repo.full_name ?? `${repo.owner}/${repo.name}`}</strong><small>{repo.private ? 'private' : 'public'} · default {repo.default_branch}</small></div>
                    <label>App<input value={draft.app} oninput={(e) => (repoConfig[repo.repository_id] = { ...draft, app: e.currentTarget.value })} maxlength="64" required /></label>
                    <label>Domain<input value={draft.domain} oninput={(e) => (repoConfig[repo.repository_id] = { ...draft, domain: e.currentTarget.value })} maxlength="253" placeholder="app.example.com" required /></label>
                    <label>Engine<input value={draft.engine_version} oninput={(e) => (repoConfig[repo.repository_id] = { ...draft, engine_version: e.currentTarget.value })} maxlength="128" required /></label>
                    <button class="btn cobalt sm" type="submit">Map</button>
                    {#if repoErrors[repo.repository_id]}<p class="inline-error" role="alert">{repoErrors[repo.repository_id]}</p>{/if}
                  </form>
                {/each}
              </div>
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
      <!-- access -->
      <section class="card">
        <div class="cardhead"><span class="label">Access</span></div>
        <div class="pad">
          <div class="access-row">
            <span class="mname">Console</span>
            <span class="tmeta num">bootstrap token session · 12h</span>
          </div>
          <div class="access-row">
            <span class="mname">Break-glass</span>
            <span class="tmeta num">cygnus --admin-socket /run/cygnus/admin.sock on the host</span>
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
              <span class="mname num">{store.node?.apps_domain ?? 'cygnus'}</span>
              <span class="tmeta num">{store.node?.version ?? '—'} · {store.node?.listen ?? '—'}</span>
            </div>
            <span class="grow"></span>
            <span class="led {store.connected ? 'live' : 'build'} breathe"></span>
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
  .sub { margin-top: 5px; font-size: 13px; color: var(--ink-3); }

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

  .install-block { border-top: 1px solid var(--line-2); padding: 14px 18px; }
  .install-form { display: grid; grid-template-columns: minmax(180px, 280px) auto; gap: 10px; align-items: end; }
  .install-form label {
    display: grid; gap: 5px;
    font-family: var(--mono); font-size: 10px; letter-spacing: 0.08em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .install-form input {
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 9px 10px; font-family: var(--mono); font-size: 12px;
  }

  .repo-list { margin-top: 14px; display: flex; flex-direction: column; gap: 10px; }
  .repo-row {
    display: grid;
    grid-template-columns: minmax(140px, 1.2fr) repeat(3, minmax(100px, 1fr)) auto;
    gap: 10px;
    align-items: end;
    padding: 12px;
    border: 1px solid var(--line-2);
    border-radius: 10px;
  }
  .repo-identity { min-width: 0; align-self: center; }
  .repo-identity strong { display: block; font-size: 12.5px; }
  .repo-identity small { display: block; margin-top: 4px; color: var(--ink-4); font-size: 10px; }
  .repo-row label {
    display: grid; gap: 4px;
    font-family: var(--mono); font-size: 9.5px; letter-spacing: 0.06em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .repo-row input {
    border: 1px solid var(--line-strong);
    border-radius: 7px; background: var(--surface); color: var(--ink);
    padding: 7px 9px; font-family: var(--mono); font-size: 11.5px;
  }

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
    .addform, .install-form { grid-template-columns: 1fr; }
    .repo-row { grid-template-columns: 1fr; }
  }
</style>
