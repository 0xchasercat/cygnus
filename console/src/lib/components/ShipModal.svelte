<script>
  import { ui } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { bytes } from '../fmt.js';
  import { collectEntries, packTarball } from '../tar.js';
  import Icon from './Icon.svelte';

  let fileInput = $state();
  let picked = $state(null); // { entries, fileCount, totalBytes, rootName }
  let appName = $state('');
  let domain = $state('');
  let engineVersion = $state('');
  // Empty = auto-detect (static site vs server entry). Never invent index.ts —
  // static apps like SvelteKit have no index.ts and would fail the build.
  let entry = $state('');
  let uploadError = $state('');
  let uploading = $state(false);
  let progress = $state(0);

  // git tab
  let githubOwner = $state('');
  let githubError = $state('');
  let githubBusy = $state(false);
  let installationId = $state('');
  let installationRepos = $state([]);
  let repoConfig = $state({});
  let repoErrors = $state({});

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
      domain = appsDomain ? `${sanitize(root) || 'app'}.${appsDomain}` : '';
      engineVersion = defaultEngine;
      uploadError = '';
    } catch (cause) {
      picked = null;
      uploadError = cause instanceof Error ? cause.message : 'Could not read that folder.';
    }
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
    progress = 0;
    uploadError = '';
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
    for (const repo of installationRepos) {
      if (!repoConfig[repo.repository_id]) {
        const eng =
          store.node?.engines?.find((en) => en.default)?.version
          ?? store.node?.engines?.[0]?.version
          ?? '';
        repoConfig[repo.repository_id] = {
          app: repo.name,
          domain: appsDomain ? `${repo.name}.${appsDomain}` : '',
          engine_version: eng,
          // Empty = auto-detect on the daemon (static vs server).
          entry: '',
        };
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
      engine_version: draft.engine_version || defaultEngine,
      // Omit empty entry so the daemon auto-detects app type.
      entry: (draft.entry ?? '').trim() || undefined,
    });
    if (!r.ok) repoErrors[repo.repository_id] = r.error ?? 'Repository configuration failed';
  }
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
                <label>App name<input bind:value={appName} maxlength="64" required /></label>
                <label>Domain<input bind:value={domain} placeholder={appsDomain ? `app.${appsDomain}` : 'app.example.com'} maxlength="253" /></label>
                <label>Engine<input bind:value={engineVersion} maxlength="128" /></label>
                <label>Entry <span class="optional">(optional — auto-detect if empty)</span><input bind:value={entry} placeholder="auto-detect" maxlength="4096" /></label>
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
              <p class="git-copy">Create the private Cygnus GitHub App, install it, then map one branch to deploy on push.</p>
              <form class="github-start" onsubmit={connectGithub}>
                <label>Organization (optional)<input bind:value={githubOwner} maxlength="39" placeholder="acme" autocomplete="organization" /></label>
                <button class="btn cobalt" type="submit" disabled={githubBusy}>{githubBusy ? 'Opening GitHub…' : 'Connect GitHub'}</button>
                {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
              </form>
            {:else}
              <div class="git-connected">
                <span class="led live"></span>
                <strong>{store.github.app?.name ?? 'Cygnus GitHub App'}</strong>
                <small>{store.github.app?.owner ?? '—'} · {store.github.repositories.length} configured</small>
              </div>
              <form class="install-form" onsubmit={discoverRepos}>
                <label>Installation ID<input bind:value={installationId} inputmode="numeric" pattern="[0-9]+" placeholder="12345678" /></label>
                <button class="btn" type="submit" disabled={githubBusy}>{githubBusy ? 'Discovering…' : 'Discover repositories'}</button>
              </form>
              {#if githubError}<p class="inline-error" role="alert">{githubError}</p>{/if}
              {#if installationRepos.length}
                <div class="repo-list">
                  {#each installationRepos as repo (repo.repository_id)}
                    {@const draft = repoConfig[repo.repository_id] ?? { app: repo.name, domain: '', engine_version: defaultEngine, entry: 'index.ts' }}
                    <form class="repo-row" onsubmit={(e) => configureRepo(e, repo)}>
                      <div class="repo-identity"><strong>{repo.full_name ?? `${repo.owner}/${repo.name}`}</strong><small>{repo.private ? 'private' : 'public'} · default {repo.default_branch}</small></div>
                      <label>App<input value={draft.app} oninput={(e) => (repoConfig[repo.repository_id] = { ...draft, app: e.currentTarget.value })} maxlength="64" required /></label>
                      <label>Domain<input value={draft.domain} oninput={(e) => (repoConfig[repo.repository_id] = { ...draft, domain: e.currentTarget.value })} maxlength="253" placeholder="app.example.com" required /></label>
                      <label>Branch<input value={repo.default_branch} disabled /></label>
                      <button class="btn cobalt sm" type="submit">Map</button>
                      {#if repoErrors[repo.repository_id]}<p class="inline-error" role="alert">{repoErrors[repo.repository_id]}</p>{/if}
                    </form>
                  {/each}
                </div>
              {:else}
                <p class="empty mono">discover an installation to list its repositories</p>
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
    margin-bottom: 14px;
  }
  .git-connected strong { font-size: 14px; }
  .git-connected small { font-size: 11px; color: var(--ink-3); }
  .install-form {
    display: grid;
    grid-template-columns: minmax(180px, 1fr) auto;
    gap: 10px;
    align-items: end;
  }
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
  .repo-list { margin-top: 14px; display: flex; flex-direction: column; gap: 10px; max-height: 320px; overflow-y: auto; }
  .repo-row {
    display: grid;
    grid-template-columns: minmax(130px, 1.2fr) repeat(3, minmax(90px, 1fr)) auto;
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
  .repo-row .inline-error { grid-column: 1 / -1; }
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
