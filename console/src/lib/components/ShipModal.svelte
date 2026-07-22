<script>
  import { ui } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { bytes } from '../fmt.js';
  import { collectEntries, packTarball } from '../tar.js';
  import Icon from './Icon.svelte';

  let fileInput = $state();
  let picked = $state(null); // { entries, fileCount, totalBytes, rootName }
  let appName = $state('');
  // Domain tracks appName until the user explicitly edits the domain field
  // themselves — after that their typed value is authoritative and never
  // silently overwritten (this also covers the app-name field being edited
  // after a folder pick, and stray browser form-autofill of a stale value).
  let domain = $state('');
  let domainTouched = $state(false);
  let engineVersion = $state('');
  // Empty = auto-detect (static site vs server entry). Never invent index.ts —
  // static apps like SvelteKit have no index.ts and would fail the build.
  let entry = $state('');
  let uploadError = $state('');
  let uploading = $state(false);
  let progress = $state(0);

  // Environment variables: array of {key, value} rows for stable editing.
  // Auto-filled from a root .env file on pick unless the operator already
  // touched the editor (mirrors the domain-follows-appName pattern above).
  let envRows = $state([]);
  let envTouched = $state(false);
  let envPasteOpen = $state(false);
  let envPasteText = $state('');

  // Preview: deploy to an isolated `<app>-<slug>` app/domain instead of
  // touching the production app. Off by default.
  let previewEnabled = $state(false);
  let previewSlug = $state('');

  // git tab
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
  const filteredRepos = $derived.by(() => {
    const q = repoQuery.trim().toLowerCase();
    if (!q) return discoverableRepos;
    return discoverableRepos.filter((repo) => {
      const full = (repo.full_name ?? `${repo.owner}/${repo.name}`).toLowerCase();
      return full.includes(q) || String(repo.owner ?? '').toLowerCase().includes(q) || String(repo.name ?? '').toLowerCase().includes(q);
    });
  });
  const selectedRepo = $derived(
    selectedRepoId == null
      ? null
      : discoverableRepos.find((r) => r.repository_id === selectedRepoId) ?? null
  );

  const appsDomain = $derived(store.node?.apps_domain ?? store.node?.apex_domain ?? '');
  const defaultEngine = $derived(
    store.node?.engines?.find((en) => en.default)?.version
    ?? store.node?.engines?.[0]?.version
    ?? ''
  );
  const live = $derived(store.mode === 'live');
  const tab = $derived(ui.shipTab ?? 'upload');

  function setTab(t) {
    ui.shipTab = t;
    uploadError = '';
    githubError = '';
  }

  function onkey(e) {
    if (e.key === 'Escape') ui.shipOpen = false;
  }

  function sanitize(s) {
    return s.toLowerCase().replace(/[^a-z0-9-]/g, '-').replace(/^-+|-+$/g, '').replace(/-{2,}/g, '-');
  }

  function onPick(e) {
    const files = e.currentTarget.files;
    if (!files || !files.length) return;
    const first = files[0];
    const rel = first.webkitRelativePath || first.name;
    const root = rel.split('/')[0] || 'app';
    try {
      const collected = collectEntries(files, root);
      picked = collected;
      appName = sanitize(root) || 'app';
      domainTouched = false;
      engineVersion = defaultEngine;
      uploadError = '';
      if (!envTouched) {
        const envFile = Array.from(files).find((file) => {
          const path = (file.webkitRelativePath || file.name).split('/').slice(1).join('/');
          return path === '.env';
        });
        if (envFile) {
          envFile.text().then((text) => {
            if (envTouched) return;
            envRows = parseEnvText(text);
          });
        } else {
          envRows = [];
        }
      }
    } catch (cause) {
      picked = null;
      uploadError = cause instanceof Error ? cause.message : 'Could not read that folder.';
    }
  }

  // Parse `KEY=VALUE` lines (dotenv-ish): skips blank lines and `#` comments,
  // strips one layer of matching quotes, keeps the first `=` as delimiter.
  function parseEnvText(text) {
    const rows = [];
    for (const rawLine of text.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line || line.startsWith('#')) continue;
      const eq = line.indexOf('=');
      if (eq <= 0) continue;
      const key = line.slice(0, eq).trim();
      let value = line.slice(eq + 1).trim();
      if (
        (value.startsWith('"') && value.endsWith('"') && value.length >= 2) ||
        (value.startsWith("'") && value.endsWith("'") && value.length >= 2)
      ) {
        value = value.slice(1, -1);
      }
      if (key) rows.push({ key, value });
    }
    return rows;
  }

  function addEnvRow() {
    envTouched = true;
    envRows = [...envRows, { key: '', value: '' }];
  }

  function updateEnvRow(index, field, value) {
    envTouched = true;
    envRows = envRows.map((row, i) => (i === index ? { ...row, [field]: value } : row));
  }

  function removeEnvRow(index) {
    envTouched = true;
    envRows = envRows.filter((_, i) => i !== index);
  }

  function applyEnvPaste() {
    envTouched = true;
    const parsed = parseEnvText(envPasteText);
    const byKey = new Map(envRows.map((row) => [row.key, row]));
    for (const row of parsed) byKey.set(row.key, row);
    envRows = Array.from(byKey.values());
    envPasteText = '';
    envPasteOpen = false;
  }

  function envRowsToMap() {
    const env = {};
    for (const row of envRows) {
      const key = row.key.trim();
      if (key) env[key] = row.value;
    }
    return env;
  }

  // Domain follows the app name field live (the folder pick is only ever a
  // fallback default) until the operator edits the domain box directly —
  // renaming the app afterward must not leave a stale domain from an earlier
  // name or a previous upload pointed at a different app.
  $effect(() => {
    if (domainTouched) return;
    domain = appName && appsDomain ? `${appName}.${appsDomain}` : '';
  });

  function onDomainInput(e) {
    domainTouched = true;
    domain = e.currentTarget.value;
  }

  function fileSummary() {
    if (!picked) return '';
    return `${picked.fileCount} files · ${bytes(picked.totalBytes)} (node_modules and .git excluded)`;
  }

  async function startUpload(e) {
    e.preventDefault();
    if (uploading || !picked || !appName) return;
    uploading = true;
    uploadError = '';
    progress = 0;
    try {
      const tarball = await packTarball(picked);
      // gzip via CompressionStream
      const cs = new CompressionStream('gzip');
      const stream = new Blob([tarball]).stream().pipeThrough(cs);
      const gzBuf = new Uint8Array(await new Response(stream).arrayBuffer());

      const r = await store.deployUpload({
        app: appName,
        domain: domain || undefined,
        // Empty means "daemon default" — never invent a version name.
        engineVersion: engineVersion.trim() || undefined,
        // Only send entry when the operator typed one. Empty → auto-detect.
        entry: entry.trim() || undefined,
        env: envRowsToMap(),
        preview: previewEnabled ? (previewSlug.trim() || undefined) : undefined,
        tarball: gzBuf,
        totalBytes: gzBuf.length,
        onProgress: (p) => (progress = p),
      });
      uploading = false;
      if (!r.ok) {
        uploadError = r.error ?? 'Upload failed';
        return;
      }
      ui.shipOpen = false;
      resetUpload();
      const { openDeploy } = await import('../stores.svelte.js');
      openDeploy(appName, r.deploymentId);
    } catch (cause) {
      uploading = false;
      uploadError = cause instanceof Error ? cause.message : 'Upload failed';
    }
  }

  function resetUpload() {
    picked = null;
    appName = '';
    domain = '';
    domainTouched = false;
    progress = 0;
    uploadError = '';
    envRows = [];
    envTouched = false;
    envPasteOpen = false;
    envPasteText = '';
    previewEnabled = false;
    previewSlug = '';
    if (fileInput) fileInput.value = '';
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
    if (githubBusy || store.github.discovering) return;
    githubBusy = true;
    githubError = '';
    const r = await store.discoverRepositories({ force: true });
    githubBusy = false;
    if (!r.ok) githubError = r.error ?? 'Repository discovery failed';
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
    if (!r.ok) {
      mapBusy = false;
      mapError = r.error ?? 'Repository configuration failed';
      return;
    }
    // Trigger initial build.
    const trigger = await store.triggerDeploy(repo.installation_id, repo.repository_id);
    mapBusy = false;
    if (!trigger.ok) {
      mapError = trigger.error ?? 'Build could not be triggered';
      return;
    }
    ui.shipOpen = false;
    clearSelectedRepo();
    await store.refreshGithub();
    // Navigate to the app detail page so the user can watch the build.
    const appName = mapDraft.app || repo.name;
    const { go } = await import('../stores.svelte.js');
    go('app', { appId: appName });
  }

  // Ensure discovery is running when the git tab is open.
  $effect(() => {
    if (!ui.shipOpen || tab !== 'git' || !store.github.configured) return;
    void store.ensureDiscoverable();
  });
</script>

<svelte:window onkeydown={ui.shipOpen ? onkey : undefined} />

{#if ui.shipOpen}
  <div
    class="scrim"
    onclick={(e) => { if (e.target === e.currentTarget) ui.shipOpen = false; }}
    role="presentation"
  >
    <div class="modal" role="dialog" aria-label="Ship an app">
      <header>
        <div class="htitle">
          <div>
            <h2>Ship to {appsDomain || 'this node'}</h2>
            <p>{live ? 'Choose how the next artifact reaches this node.' : 'Preview dataset · daemon bridge offline.'}</p>
          </div>
        </div>
        <button class="btn icon sm" onclick={() => (ui.shipOpen = false)} aria-label="Close">
          <Icon name="x" size={14} />
        </button>
      </header>

      {#if live}
        <div class="tabs">
          <button class:on={tab === 'upload'} onclick={() => setTab('upload')}>Upload a folder</button>
          <button class:on={tab === 'git'} onclick={() => setTab('git')}>Connect Git</button>
        </div>

        {#if tab === 'upload'}
          <div class="upload">
            <input bind:this={fileInput} type="file" webkitdirectory onchange={onPick} hidden />
            <button class="picker" onclick={() => fileInput?.click()} disabled={uploading}>
              <span class="cicon"><Icon name="folder" size={19} /></span>
              <span class="cname">{picked ? 'Choose a different folder' : 'Choose a folder'}</span>
              <span class="cdesc">Packed as a tarball, gzipped, uploaded in 1 MiB chunks.</span>
            </button>

            {#if picked}
              <form class="uform" onsubmit={startUpload}>
                <label>App name<input bind:value={appName} maxlength="64" autocomplete="off" required /></label>
                <label>Domain<input value={domain} oninput={onDomainInput} placeholder={appsDomain ? `app.${appsDomain}` : 'app.example.com'} maxlength="253" autocomplete="off" /></label>
                <label>Engine<input bind:value={engineVersion} maxlength="128" autocomplete="off" /></label>
                <label>Entry <span class="optional">(optional — auto-detect if empty)</span><input bind:value={entry} placeholder="auto-detect" maxlength="4096" autocomplete="off" /></label>

                <label class="preview-toggle">
                  <input type="checkbox" bind:checked={previewEnabled} />
                  Deploy as a preview (isolated app + domain)
                </label>
                {#if previewEnabled}
                  <label>Preview slug <span class="optional">(e.g. a branch or PR name)</span>
                    <input bind:value={previewSlug} placeholder="pr-42" maxlength="64" autocomplete="off" required={previewEnabled} />
                  </label>
                {/if}

                <div class="env-editor">
                  <div class="env-header">
                    <span>Environment variables</span>
                    <div class="env-header-actions">
                      <button class="btn sm" type="button" onclick={() => (envPasteOpen = !envPasteOpen)}>Paste .env</button>
                      <button class="btn sm" type="button" onclick={addEnvRow}>Add</button>
                    </div>
                  </div>
                  {#if envPasteOpen}
                    <div class="env-paste">
                      <textarea bind:value={envPasteText} placeholder={'KEY=value\nANOTHER_KEY=value'} rows="4"></textarea>
                      <div class="env-paste-actions">
                        <button class="btn sm" type="button" onclick={() => { envPasteOpen = false; envPasteText = ''; }}>Cancel</button>
                        <button class="btn cobalt sm" type="button" onclick={applyEnvPaste} disabled={!envPasteText.trim()}>Apply</button>
                      </div>
                    </div>
                  {/if}
                  {#if envRows.length}
                    <div class="env-rows">
                      {#each envRows as row, i (i)}
                        <div class="env-row">
                          <input value={row.key} oninput={(e) => updateEnvRow(i, 'key', e.currentTarget.value)} placeholder="KEY" maxlength="128" autocomplete="off" spellcheck="false" />
                          <input value={row.value} oninput={(e) => updateEnvRow(i, 'value', e.currentTarget.value)} placeholder="value" maxlength="4096" autocomplete="off" spellcheck="false" />
                          <button class="btn icon sm" type="button" onclick={() => removeEnvRow(i)} aria-label="Remove {row.key || 'env var'}">
                            <Icon name="x" size={12} />
                          </button>
                        </div>
                      {/each}
                    </div>
                  {:else}
                    <p class="empty mono">no env vars · add one or paste a .env file</p>
                  {/if}
                </div>

                <span class="summary num">{fileSummary()}</span>
                {#if uploading}
                  <div class="progress"><i style="width:{Math.round(progress * 100)}%"></i></div>
                  <span class="summary num">
                    {progress < 1
                      ? `Uploading source · ${Math.round(progress * 100)}%`
                      : 'Queued — opening build…'}
                  </span>
                {/if}
                {#if uploadError}<p class="inline-error" role="alert">{uploadError}</p>{/if}
                <div class="uactions">
                  <button class="btn" type="button" onclick={resetUpload} disabled={uploading}>Clear</button>
                  <button class="btn cobalt" type="submit" disabled={uploading || !appName}>
                    {uploading
                      ? progress < 1
                        ? 'Uploading…'
                        : 'Starting build…'
                      : 'Build & deploy'}
                  </button>
                </div>
              </form>
            {/if}
          </div>
        {:else}
          <div class="git">
            {#if !store.github.configured}
              <p class="git-copy">Create the private Cygnus GitHub App, install it, then connect a repo to deploy.</p>
              <form class="github-start" onsubmit={connectGithub}>
                <label>Organization (optional)<input bind:value={githubOwner} maxlength="39" placeholder="acme" autocomplete="organization" /></label>
                <button class="btn cobalt" type="submit" disabled={githubBusy}>{githubBusy ? 'Opening GitHub…' : 'Connect GitHub'}</button>
                {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
              </form>
            {:else}
              <div class="git-connected">
                <span class="led live"></span>
                <div class="git-meta">
                  <strong>{store.github.app?.name ?? 'Cygnus GitHub App'}</strong>
                  <small>{store.github.app?.owner ?? '—'} · {store.github.repositories.length} mapped · {installationCount} install{installationCount === 1 ? '' : 's'}</small>
                </div>
                <button class="btn sm" type="button" onclick={refreshDiscoverable} disabled={githubBusy || store.github.discovering}>
                  {githubBusy || store.github.discovering ? 'Refreshing…' : 'Refresh'}
                </button>
              </div>
              {#if store.github.app?.html_url && !installationCount && store.github.discovered && !store.github.discovering}
                <a class="btn cobalt sm install-link" href="{store.github.app.html_url}/installations/new">
                  Install App on GitHub <Icon name="arrowR" size={12} />
                </a>
              {/if}
              {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}

              {#if selectedRepo}
                <form class="map-panel" onsubmit={configureSelected}>
                  <div class="map-head">
                    <div class="repo-identity">
                      <strong>{selectedRepo.full_name ?? `${selectedRepo.owner}/${selectedRepo.name}`}</strong>
                      <small>{selectedRepo.private ? 'private' : 'public'} · {selectedRepo.default_branch}</small>
                    </div>
                    <button class="btn sm" type="button" onclick={clearSelectedRepo}>Back</button>
                  </div>
                  <div class="repo-fields">
                    <label>App<input bind:value={mapDraft.app} maxlength="64" required /></label>
                    <label>Domain<input bind:value={mapDraft.domain} maxlength="253" placeholder="app.example.com" required /></label>
                  </div>
                  {#if mapError}<p class="inline-error" role="alert">{mapError}</p>{/if}
                  <div class="map-actions">
                    <button class="btn cobalt sm" type="submit" disabled={mapBusy}>
                      {mapBusy ? 'Deploying…' : 'Deploy'}
                    </button>
                  </div>
                </form>
              {:else if discoverableRepos.length}
                <div class="repo-search">
                  <input type="search" bind:value={repoQuery} placeholder="Search repositories…" aria-label="Search repositories" />
                  <span class="repo-search-count num">{filteredRepos.length}/{discoverableRepos.length}</span>
                </div>
                <div class="repo-pick-list">
                  {#each filteredRepos as repo (repo.repository_id)}
                    {@const alreadyMapped = store.github.repositories.some((r) => r.repository_id === repo.repository_id)}
                    <button type="button" class="repo-pick" class:mapped={alreadyMapped} onclick={() => selectRepo(repo)}>
                      <div class="repo-identity">
                        <strong>{repo.full_name ?? `${repo.owner}/${repo.name}`}</strong>
                        <small>{repo.private ? 'private' : 'public'} · {repo.default_branch}{#if alreadyMapped} · mapped{/if}</small>
                      </div>
                      <Icon name="arrowR" size={12} />
                    </button>
                  {:else}
                    <p class="empty mono">no repositories match “{repoQuery}”</p>
                  {/each}
                </div>
              {:else if store.github.discovering || githubBusy || !store.github.discovered}
                <p class="empty mono">discovering repositories…</p>
              {:else if installationCount === 0}
                <p class="empty mono">install the GitHub App on an account to see repositories</p>
              {:else}
                <p class="empty mono">no repositories accessible to this installation</p>
              {/if}
            {/if}
          </div>
        {/if}
      {:else}
        <div class="choices">
          <button class="choice" disabled title="Unavailable: daemon admin bridge offline">
            <span class="cicon"><Icon name="branch" size={19} /></span>
            <span class="cname">Connect Git</span>
            <span class="cdesc">Push-to-deploy requires the daemon admin bridge.</span>
          </button>
          <button class="choice" disabled title="Unavailable: daemon admin bridge offline">
            <span class="cicon"><Icon name="folder" size={19} /></span>
            <span class="cname">Upload a folder</span>
            <span class="cdesc">Source upload requires the daemon admin bridge.</span>
          </button>
        </div>
        <div class="offline-note" role="status">
          <span class="led preview" aria-hidden="true"></span>
          <div>
            <b>Preview dataset · daemon bridge offline</b>
            <span>No deploy, import, or upload was started. Connect a daemon to enable shipping.</span>
          </div>
        </div>
      {/if}

      <footer>
        <span class="fcli num"><i>$</i> {live ? 'live · operator' : 'tenant 0 · preview'}</span>
        <span class="fnote">{live ? 'uploads run server-side builds' : 'shipping disabled · daemon bridge offline'}</span>
      </footer>
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
    align-items: flex-start;
    justify-content: center;
    animation: scrim-in 0.16s ease both;
  }
  @keyframes scrim-in { from { opacity: 0; } }

  .modal {
    margin-top: 12vh;
    width: min(620px, calc(100vw - 48px));
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 22px;
    box-shadow: var(--shadow-pop);
    padding: 26px 26px 0;
    animation: pal-in 0.22s cubic-bezier(0.22, 1, 0.36, 1) both;
  }
  @keyframes pal-in {
    from { opacity: 0; transform: translateY(10px) scale(0.985); }
  }

  header {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    margin-bottom: 18px;
  }
  .htitle { display: flex; gap: 12px; align-items: flex-start; }
  h2 { font-size: 18px; font-weight: 650; letter-spacing: -0.015em; }
  header p { margin-top: 5px; font-size: 13px; color: var(--ink-2); }

  .tabs {
    display: inline-flex;
    gap: 2px;
    padding: 3px;
    background: #eceef2;
    border-radius: 11px;
    margin-bottom: 18px;
  }
  .tabs button {
    height: 30px;
    padding: 0 14px;
    border-radius: 8px;
    font-size: 12.5px;
    font-weight: 600;
    color: var(--ink-2);
  }
  .tabs button.on {
    background: var(--surface);
    color: var(--ink);
    box-shadow: 0 1px 2px rgba(13, 18, 28, 0.08), 0 4px 10px -4px rgba(13, 18, 28, 0.1);
  }

  /* upload */
  .picker {
    width: 100%;
    display: grid;
    grid-template-columns: 38px 1fr;
    gap: 4px 14px;
    align-items: center;
    padding: 16px 18px;
    border: 1px dashed var(--line-strong);
    border-radius: 14px;
    background: var(--surface-2);
    text-align: left;
    transition: border-color 0.14s ease, background 0.14s ease;
  }
  .picker:hover { border-color: var(--cobalt); background: var(--cobalt-ghost); }
  .picker:disabled { opacity: 0.6; cursor: wait; }
  .cicon {
    width: 38px; height: 38px; border-radius: 12px;
    background: var(--surface-3); color: var(--ink);
    display: grid; place-items: center; grid-row: span 2;
  }
  .picker:hover .cicon { background: var(--cobalt-ghost); color: var(--cobalt-deep); }
  .cname { font-size: 14px; font-weight: 650; letter-spacing: -0.01em; }
  .cdesc { grid-column: 2; font-size: 12px; color: var(--ink-3); }

  .uform {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 12px;
    margin-top: 16px;
  }
  .uform label {
    display: grid; gap: 5px;
    font-family: var(--mono); font-size: 10px; letter-spacing: 0.08em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .uform .optional {
    text-transform: none;
    letter-spacing: 0;
    color: var(--ink-4);
    font-weight: 400;
  }
  .uform input {
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 9px 10px; font-family: var(--mono); font-size: 12px;
  }
  .summary { grid-column: 1 / -1; font-size: 11px; color: var(--ink-3); }
  .progress {
    grid-column: 1 / -1;
    height: 4px;
    background: var(--surface-3);
    border-radius: 4px;
    overflow: hidden;
  }
  .progress i { display: block; height: 100%; background: var(--cobalt); border-radius: 4px; transition: width 0.18s ease; }
  .uactions {
    grid-column: 1 / -1;
    display: flex;
    justify-content: flex-end;
    gap: 10px;
    margin-top: 4px;
  }
  .inline-error { grid-column: 1 / -1; color: var(--red); font-size: 11.5px; margin: 2px 0 0; }

  .preview-toggle {
    grid-column: 1 / -1;
    display: flex !important;
    flex-direction: row;
    align-items: center;
    gap: 8px;
    text-transform: none !important;
    letter-spacing: 0 !important;
    font-size: 12px !important;
    color: var(--ink-2) !important;
    cursor: pointer;
  }
  .preview-toggle input[type='checkbox'] { width: 14px; height: 14px; accent-color: var(--cobalt); }

  .env-editor { grid-column: 1 / -1; display: grid; gap: 8px; }
  .env-header {
    display: flex; align-items: center; justify-content: space-between;
    font-family: var(--mono); font-size: 10px; letter-spacing: 0.08em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .env-header-actions { display: flex; gap: 6px; }
  .env-rows { display: grid; gap: 6px; }
  .env-row { display: grid; grid-template-columns: 1fr 1fr auto; gap: 6px; align-items: center; }
  .env-row input {
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 8px 9px; font-family: var(--mono); font-size: 12px;
  }
  .env-paste { display: grid; gap: 6px; }
  .env-paste textarea {
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 9px 10px; font-family: var(--mono); font-size: 12px; resize: vertical;
  }
  .env-paste-actions { display: flex; justify-content: flex-end; gap: 8px; }
  .env-editor .empty { margin: 0; font-size: 11.5px; color: var(--ink-4); }

  /* git */
  .git-copy { margin: 0 0 14px; font-size: 13px; color: var(--ink-2); line-height: 1.5; }
  .github-start { display: grid; gap: 12px; max-width: 360px; }
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
  .git-connected {
    display: flex; align-items: center; gap: 10px;
    padding: 12px 14px; border: 1px solid var(--line-2); border-radius: 11px;
    margin-bottom: 14px; min-width: 0;
  }
  .git-meta { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; }
  .git-connected strong { font-size: 14px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .git-connected small { font-size: 11px; color: var(--ink-3); }
  .install-link { display: inline-flex; margin-bottom: 12px; }

  .repo-search {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 10px;
    align-items: center;
    margin-bottom: 10px;
  }
  .repo-search input {
    width: 100%; min-width: 0; box-sizing: border-box;
    border: 1px solid var(--line-strong);
    border-radius: 8px; background: var(--surface); color: var(--ink);
    padding: 9px 10px; font-family: var(--mono); font-size: 12px;
  }
  .repo-search-count { font-size: 11px; color: var(--ink-3); white-space: nowrap; }

  .repo-pick-list {
    display: flex; flex-direction: column; gap: 6px;
    max-height: 280px; overflow-y: auto; min-width: 0;
  }
  .repo-pick {
    display: flex; align-items: center; justify-content: space-between; gap: 12px;
    width: 100%; text-align: left;
    padding: 11px 12px;
    border: 1px solid var(--line-2); border-radius: 10px;
    background: var(--surface); color: var(--ink);
    cursor: pointer; min-width: 0;
  }
  .repo-pick:hover { border-color: var(--cobalt); background: var(--cobalt-ghost); }
  .repo-pick.mapped { opacity: 0.72; }
  .repo-identity { min-width: 0; flex: 1; }
  .repo-identity strong { display: block; font-size: 12.5px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .repo-identity small { display: block; margin-top: 4px; color: var(--ink-4); font-size: 10px; }

  .map-panel {
    display: flex; flex-direction: column; gap: 12px;
    padding: 12px; border: 1px solid var(--line-2); border-radius: 10px; min-width: 0;
  }
  .map-head { display: flex; align-items: center; justify-content: space-between; gap: 12px; min-width: 0; }
  .repo-fields { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 10px; min-width: 0; }
  .repo-fields label {
    display: grid; gap: 4px; min-width: 0;
    font-family: var(--mono); font-size: 9.5px; letter-spacing: 0.06em;
    text-transform: uppercase; color: var(--ink-3);
  }
  .repo-fields input {
    width: 100%; min-width: 0; box-sizing: border-box;
    border: 1px solid var(--line-strong);
    border-radius: 7px; background: var(--surface); color: var(--ink);
    padding: 7px 9px; font-family: var(--mono); font-size: 11.5px;
  }
  .map-actions { display: flex; justify-content: flex-end; }
  .empty { padding: 22px 0; text-align: center; font-size: 11px; color: var(--ink-4); letter-spacing: 0.06em; }

  /* preview chooser */
  .choices { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
  .choice {
    position: relative;
    display: flex; flex-direction: column; align-items: flex-start; gap: 8px;
    padding: 18px 18px 16px;
    border: 1px solid var(--line); border-radius: 16px;
    background: var(--surface); text-align: left;
    cursor: not-allowed; opacity: 0.7;
  }
  .cicon {
    width: 38px; height: 38px; border-radius: 12px;
    background: var(--surface-3); color: var(--ink-3);
    display: grid; place-items: center; margin-bottom: 2px;
  }
  .cname { font-size: 14.5px; font-weight: 650; }
  .cdesc { font-size: 12px; line-height: 1.55; color: var(--ink-3); }

  .offline-note {
    display: flex; align-items: flex-start; gap: 10px;
    margin-top: 15px; padding: 12px 13px;
    border: 1px solid var(--violet-soft); border-radius: 11px;
    background: var(--violet-soft); color: var(--ink-2);
  }
  .offline-note .led { margin-top: 5px; }
  .offline-note div { display: flex; flex-direction: column; gap: 2px; }
  .offline-note b { font-size: 12px; font-weight: 650; color: var(--ink); }
  .offline-note span:not(.led) { font-size: 11.5px; line-height: 1.45; }

  footer {
    display: flex; align-items: center; gap: 10px;
    margin: 20px -26px 0; padding: 13px 26px;
    border-top: 1px solid var(--line-2); background: var(--surface-2);
    border-radius: 0 0 22px 22px;
  }
  .fcli { font-size: 12px; color: var(--ink); }
  .fcli i { font-style: normal; color: var(--ink-4); margin-right: 7px; }
  .fnote { margin-left: auto; font-size: 11px; color: var(--ink-4); }
</style>
