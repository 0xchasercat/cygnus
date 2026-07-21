<script>
  import { ui, openDeploy, go } from '../stores.svelte.js';
  import { store } from '../live.svelte.js';
  import { relativeTime } from '../time.js';
  import { bytes, millis, rate, shortHash } from '../fmt.js';
  import Identicon from '../components/Identicon.svelte';
  import Icon from '../components/Icon.svelte';
  import Spark from '../components/Spark.svelte';

  const app = $derived(store.appByName(ui.appId) ?? store.apps[0] ?? null);
  const appDeploys = $derived(app ? store.deploymentsFor(app.name) : []);
  const current = $derived(
    app?.active ? appDeploys.find((d) => d.id === app.active.deployment_id) ?? appDeploys[0] : appDeploys[0] ?? null
  );
  const am = $derived(app ? store.appMetrics(app.name) : null);

  const LED = { active: 'live', building: 'build', failed: 'fail', sealed: 'cold' };
  const STATUS = { active: 'live', building: 'building', failed: 'failed', sealed: 'sealed' };

  const stateLed = $derived(
    app
      ? app.lifecycle_state === 'building'
        ? 'build'
        : app.lifecycle_state === 'cold'
          ? 'cold'
          : app.name.startsWith('pr-')
            ? 'preview'
            : 'live'
      : 'cold'
  );

  // ——— domains ———
  // tenant-0 (the console) has no rows in the domains table by design — its
  // public hostname is edge.dashboard_domain, set from Settings. Show that
  // instead of the generic custom-domain list so the card never looks empty
  // for an app that in fact has automatic HTTPS configured.
  const isTenantZero = $derived(app?.name === 'tenant-0');
  const dashboardDomainView = $derived(
    isTenantZero && store.node?.dashboard_domain
      ? {
          host: store.node.dashboard_domain,
          kind: 'native',
          tls: store.node.ssl_mode === 'acme' ? 'acme' : 'self_signed',
          status: store.node.ssl_mode === 'acme' ? 'active' : 'fallback_active',
          dns: null,
        }
      : null,
  );
  // Live domain list comes from store.appDomains (cached) once the app is open.
  // In preview the fixtures seed a per-app domain map so the card renders.
  // tenant-0 substitutes its dashboard domain in place of the (always empty)
  // custom-domain list.
  const domains = $derived(
    isTenantZero
      ? (dashboardDomainView ? [dashboardDomainView] : [])
      : app
        ? (store.appDomains(app.name) ?? previewAppDomains(app.name))
        : null,
  );

  const DOMAIN_PILL = {
    active: { cls: 'live', text: 'active' },
    fallback_active: { cls: 'live', text: 'ready' },
    issuing: { cls: 'cobalt', text: 'issuing' },
    pending: { cls: 'ghost', text: 'pending' },
    failed: { cls: 'fail', text: 'failed' },
  };

  function domainPill(d) {
    // Local native domains that already resolve shouldn't look broken.
    if (d.kind === 'native' && (d.status === 'pending' || d.status === 'fallback_active') && d.dns?.ok) {
      return { cls: 'live', text: d.status === 'pending' ? 'local' : 'ready' };
    }
    return DOMAIN_PILL[d.status] ?? { cls: 'ghost', text: d.status ?? '—' };
  }

  function isLocalHost(host) {
    if (!host) return false;
    const h = String(host).toLowerCase().replace(/\.$/, '');
    return h === 'localhost' || h.endsWith('.localhost') || h === '127.0.0.1';
  }

  function appUrl(host) {
    if (!host) return '#';
    // Prefer https when the node has a TLS listener or the host is local
    // (we issue a self-signed cert for apps.localhost).
    const preferHttps = !!store.node?.https_listen || isLocalHost(host) || store.node?.ssl_mode === 'acme';
    return `${preferHttps ? 'https' : 'http'}://${host}`;
  }

  let addDomainOpen = $state(false);
  let newHost = $state('');
  let domainError = $state('');
  let domainBusy = $state(false);
  let hostEl = $state();
  let pendingTls = $state({}); // host -> true while toggling

  // Fetch the live domain list when an app is open (and again when navigating
  // between apps). Preview mode skips the fetch — fixtures render the card.
  $effect(() => {
    const name = app?.name;
    if (name && store.mode === 'live') {
      store.refreshAppDomains(name);
    }
  });

  // ——— environment variables ———
  let newEnvKey = $state('');
  let newEnvValue = $state('');
  let envVarBusy = $state(false);
  let envVarError = $state('');
  let envVarPending = $state({}); // key -> true while removing
  let envValueVisible = $state({}); // key -> true while revealed

  // Live values come from store.envVars (fetched below). Preview mode has
  // no values to show (they're sealed secrets in the fixture data) — render
  // its known keys as an unrevealable placeholder so the card isn't empty.
  const envVarEntries = $derived(
    app
      ? store.mode === 'live'
        ? Object.entries(store.envVars(app.name) ?? {})
        : (app.env_keys ?? []).map((key) => [key, '••••••••'])
      : [],
  );

  $effect(() => {
    const name = app?.name;
    if (name && store.mode === 'live') {
      store.refreshEnvVars(name);
    }
  });

  async function submitEnvVar(e) {
    e.preventDefault();
    if (envVarBusy || !app) return;
    const key = newEnvKey.trim();
    if (!key) return;
    envVarBusy = true;
    envVarError = '';
    const r = await store.setEnvVar(app.name, key, newEnvValue);
    envVarBusy = false;
    if (!r.ok) {
      envVarError = r.error ?? 'Could not set env var';
      return;
    }
    newEnvKey = '';
    newEnvValue = '';
  }

  async function removeEnvVarRow(key) {
    if (!app) return;
    envVarPending = { ...envVarPending, [key]: true };
    const r = await store.removeEnvVar(app.name, key);
    envVarPending = { ...envVarPending, [key]: false };
    if (!r.ok) envVarError = r.error ?? 'Could not remove env var';
  }

  async function submitDomain(e) {
    e.preventDefault();
    if (domainBusy || !app) return;
    const host = newHost.trim().toLowerCase();
    if (!host) return;
    if (!/^[a-z0-9]([a-z0-9.-]*[a-z0-9])?$/i.test(host) || !host.includes('.')) {
      domainError = 'Enter a domain like app.example.com (letters, digits, dots, hyphens).';
      return;
    }
    domainBusy = true;
    domainError = '';
    const r = await store.addDomain(app.name, host);
    domainBusy = false;
    if (!r.ok) {
      domainError = r.error ?? 'Could not add domain';
      return;
    }
    newHost = '';
    addDomainOpen = false;
  }

  async function removeHost(d) {
    if (!app) return;
    pendingTls = { ...pendingTls, [d.host + ':rm']: true };
    const r = await store.removeDomain(app.name, d.host);
    pendingTls = { ...pendingTls, [d.host + ':rm']: false };
    if (!r.ok) domainError = r.error ?? 'Could not remove domain';
  }

  async function toggleTls(d) {
    if (!app) return;
    const next = d.tls === 'acme' ? 'self_signed' : 'acme';
    pendingTls = { ...pendingTls, [d.host]: true };
    const r = await store.setDomainTls(app.name, d.host, next);
    pendingTls = { ...pendingTls, [d.host]: false };
    if (!r.ok) domainError = r.error ?? 'Could not change TLS';
  }

  async function setPrimary(d) {
    if (!app || d.is_primary) return;
    pendingTls = { ...pendingTls, [d.host + ':primary']: true };
    const r = await store.setPrimaryDomain(app.name, d.host);
    pendingTls = { ...pendingTls, [d.host + ':primary']: false };
    if (!r.ok) domainError = r.error ?? 'Could not set primary domain';
  }

  async function retryAcme(d) {
    if (!app) return;
    pendingTls = { ...pendingTls, [d.host + ':retry']: true };
    const r = await store.retryDomainAcme(app.name, d.host);
    pendingTls = { ...pendingTls, [d.host + ':retry']: false };
    if (!r.ok) domainError = r.error ?? 'Could not retry certificate issuance';
  }

  // Preview-mode per-app domain fixtures keyed by app name. The live store
  // replaces these with real data once the backend branch lands; until then
  // the card renders the same shape.
  function previewAppDomains(name) {
    return PREVIEW_DOMAINS[name] ?? null;
  }

  // rollback
  let rollbackOpen = $state(false);
  let rollbackTarget = $state(null);
  let rollbackError = $state('');
  let rollbackBusy = $state(false);

  const rollbackCandidates = $derived(
    app ? appDeploys.filter((d) => d.status === 'sealed' && d.id !== app?.active?.deployment_id) : []
  );

  function askRollback() {
    if (!rollbackCandidates.length) {
      rollbackError = 'No prior sealed deployment is available to roll back to.';
      rollbackOpen = true;
      return;
    }
    rollbackError = '';
    rollbackOpen = true;
    rollbackTarget = rollbackCandidates[0]?.id ?? null;
  }

  async function confirmRollback() {
    if (!app || !rollbackTarget || rollbackBusy) return;
    rollbackBusy = true;
    rollbackError = '';
    const target = appDeploys.find((d) => d.id === rollbackTarget);
    const expected = app.active?.artifact_hash ?? target?.artifact_hash ?? '';
    const r = await store.rollback(app.name, rollbackTarget, expected);
    rollbackBusy = false;
    if (!r.ok) {
      rollbackError = r.error ?? 'Rollback failed';
      return;
    }
    rollbackOpen = false;
    rollbackTarget = null;
  }

  function fmtIdle(ms) {
    if (!ms) return 'pinned warm';
    return `${Math.round(ms / 60000)}m idle`;
  }

  // Per-app preview domain fixtures. Mixed statuses so the card shows a green
  // active native domain, a yellow fallback_active custom domain (DNS not
  // pointed here yet), and one issuing. The live store overrides these.
  const PREVIEW_DOMAINS = {
    atelier: [
      { host: 'atelier.swan.host', kind: 'native', tls: 'acme', status: 'active', dns: { expected_ip: '203.0.113.10', resolves_to: '203.0.113.10', ok: true }, expires_unix: 1762675200 },
      { host: 'atelier.studio', kind: 'custom', tls: 'acme', status: 'active', dns: { expected_ip: '203.0.113.10', resolves_to: '203.0.113.10', ok: true }, expires_unix: 1761062400 },
      { host: 'shop.atelier.dev', kind: 'custom', tls: 'acme', status: 'fallback_active', dns: { expected_ip: '203.0.113.10', resolves_to: '198.51.100.4', ok: false } },
    ],
    'helios-api': [
      { host: 'helios-api.swan.host', kind: 'native', tls: 'acme', status: 'active', dns: { expected_ip: '203.0.113.10', resolves_to: '203.0.113.10', ok: true }, expires_unix: 1762675200 },
      { host: 'helios.dev', kind: 'custom', tls: 'acme', status: 'issuing', dns: { expected_ip: '203.0.113.10', resolves_to: '203.0.113.10', ok: true } },
    ],
  };
</script>

{#if app}
  <div class="page screen-enter">
    <!-- ————— header, boxless ————— -->
    <header class="head">
      <Identicon name={app.name} size={46} />
      <div class="title">
        <div class="row1">
          <h1>{app.name}</h1>
          <span class="led {stateLed}" class:breathe={app.lifecycle_state === 'ready'}></span>
          <span class="pill {app.name.startsWith('pr-') ? 'preview' : 'ghost'}">{app.name.startsWith('pr-') ? 'preview' : 'production'}</span>
        </div>
        <div class="domains num">
          {#if isTenantZero}
            {#if store.node?.dashboard_domain}
              <a href={appUrl(store.node.dashboard_domain)} target="_blank" rel="noopener noreferrer" class="dom"
                >{store.node.dashboard_domain} <Icon name="ext" size={11} /></a
              >
            {/if}
          {:else}
            {#each app.domains as d}
              <a href={appUrl(d)} target="_blank" rel="noopener noreferrer" class="dom"
                >{d} <Icon name="ext" size={11} /></a
              >
              <span class="dot">·</span>
            {/each}
          {/if}
        </div>
      </div>
      <div class="actions">
        <button class="btn" onclick={() => go('observe', { observeAppFilter: app.name })}>
          <Icon name="observe" size={13} />Observe
        </button>
        {#if isTenantZero}
          {#if store.node?.dashboard_domain}
            <a class="btn primary" href={appUrl(store.node.dashboard_domain)} target="_blank" rel="noopener noreferrer"><Icon name="ext" size={13} />Visit</a>
          {/if}
        {:else if app.domains?.length}
          <a class="btn primary" href={appUrl(app.domains[0])} target="_blank" rel="noopener noreferrer"><Icon name="ext" size={13} />Visit</a>
        {/if}
      </div>
    </header>

    <div class="grid">
      <div class="main">
        <!-- ————— current artifact ————— -->
        <section class="card prod">
          <div class="prodhead">
            <span class="label">{app.name.startsWith('pr-') ? 'Preview' : 'Production'}</span>
            {#if current}
              <span class="pill {LED[current.status] === 'cold' ? 'ghost' : LED[current.status]}">{STATUS[current.status] ?? current.status}</span>
            {:else}
              <span class="pill ghost">no artifact</span>
            {/if}
          </div>
          {#if current}
            <button class="commit" onclick={() => openDeploy(app.name, current.id)}>
              {current.source?.branch ? `${current.source.branch} · ${current.source.commit ?? shortHash(current.source_hash)}` : shortHash(current.source_hash)}
            </button>
            <div class="meta num">
              {current.id} · {relativeTime(current.created_ms)}
              {#if current.artifact_hash}· artifact {shortHash(current.artifact_hash)}{/if}
            </div>
          {:else}
            <p class="commit dim">No active artifact yet.</p>
          {/if}
          <div class="hairline-h"></div>
          <div class="prodfoot">
            <div class="stat">
              <span class="label">p50</span>
              <span class="readout md">{am ? millis(am.p50_ms) : '— ms'}</span>
            </div>
            <div class="stat">
              <span class="label">p99</span>
              <span class="readout md">{am ? millis(am.p99_ms) : '— ms'}</span>
            </div>
            <div class="stat">
              <span class="label">Requests</span>
              <span class="readout md">{am ? rate(am.rps_1m) : '—'}<span class="unit">rps</span></span>
            </div>
            <div class="grow"></div>
            {#if app.name.startsWith('pr-')}
              <span class="pill ghost">preview</span>
            {:else}
              <button class="btn" onclick={askRollback}><Icon name="rollback" size={14} />Roll back</button>
            {/if}
            {#if current}
              <button class="btn" onclick={() => openDeploy(app.name, current.id)}>Build log</button>
            {/if}
          </div>
        </section>

        <!-- ————— deploy timeline ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Deploys</span>
            {#if appDeploys.some((_, i) => i)}
              <span class="histbars"><Spark data={store.appRequestSeries(app.name)} w={120} h={16} color="var(--ink-3)" /></span>
            {/if}
          </div>
          {#if appDeploys.length}
            <div class="rows">
              {#each appDeploys as d (d.id)}
                <button class="drow" onclick={() => openDeploy(app.name, d.id)}>
                  <span class="led {LED[d.status] ?? 'cold'}" class:breathe={d.status === 'building'}></span>
                  <span class="dcommit">{d.source?.branch ? `${d.source.branch} · ${d.source.commit ?? shortHash(d.source_hash)}` : shortHash(d.source_hash)}</span>
                  <span class="chip"><Icon name="branch" size={11} />{d.source?.branch ?? '—'}</span>
                  <span class="dnum num when">{relativeTime(d.created_ms)}</span>
                  <span class="chev"><Icon name="chevR" size={13} /></span>
                </button>
              {/each}
            </div>
          {:else}
            <div class="empty mono">no deployments yet</div>
          {/if}
        </section>
      </div>

      <aside class="side">
        <!-- ————— domains ————— -->
        <section class="card">
          <div class="cardhead">
            <span
              class="label"
              title={isTenantZero
                ? 'The console domain is managed from Settings → Dashboard domain.'
                : 'Native domain is always present · custom domains issue a certificate once DNS resolves.'}
            >Domains</span>
            {#if !isTenantZero}
              <button class="btn sm" onclick={() => (addDomainOpen = !addDomainOpen)}>
                <Icon name="plus" size={12} />{addDomainOpen ? 'Cancel' : 'Add domain'}
              </button>
            {/if}
          </div>

          {#if addDomainOpen && !isTenantZero}
            <form class="dom-add" onsubmit={submitDomain}>
              <input
                bind:this={hostEl}
                bind:value={newHost}
                type="text"
                autocapitalize="off"
                spellcheck="false"
                maxlength="253"
                placeholder="app.example.com"
                required
              />
              <button class="btn cobalt sm" type="submit" disabled={domainBusy || !newHost.trim()}>
                {domainBusy ? 'Adding…' : 'Add'}
              </button>
              {#if domainError}<p class="dom-err" role="alert">{domainError}</p>{/if}
            </form>
          {/if}

          {#if domains && domains.length}
            <div class="dom-list">
              {#each domains as d (d.host)}
                {@const pill = domainPill(d)}
                {@const removing = pendingTls[d.host + ':rm']}
                {@const toggling = pendingTls[d.host]}
                {@const settingPrimary = pendingTls[d.host + ':primary']}
                {@const retrying = pendingTls[d.host + ':retry']}
                <div class="dom-row" class:native={d.kind === 'native'}>
                  <div class="dom-host">
                    <a href={appUrl(d.host)} target="_blank" rel="noopener noreferrer" class="dh-link num">{d.host}</a>
                    <span class="dom-tag">{d.kind}</span>
                    {#if d.is_primary}
                      <span class="dom-tag primary" title="Primary domain for this app">primary</span>
                    {:else if !isTenantZero}
                      <button
                        type="button"
                        class="linklike dom-primary-set"
                        onclick={() => setPrimary(d)}
                        disabled={settingPrimary}
                      >
                        {settingPrimary ? 'setting…' : 'set primary'}
                      </button>
                    {/if}
                  </div>
                  <span
                    class="pill {pill.cls}"
                    title={d.kind === 'native' && isLocalHost(d.host)
                      ? 'Native local domain · served on this node'
                      : d.status === 'fallback_active'
                        ? 'Using a self-signed certificate until DNS resolves for ACME'
                        : ''}
                  >
                    {pill.text}
                  </span>
                  <div class="dom-actions">
                    {#if isTenantZero}
                      <button
                        type="button"
                        class="tls-toggle {d.tls === 'acme' ? 'on' : ''}"
                        onclick={() => go('settings')}
                        aria-label="Manage in settings"
                        title="Managed from Settings → Dashboard domain"
                      >
                        <span class="tls-dot"></span>
                      </button>
                    {:else}
                      <button
                        type="button"
                        class="tls-toggle {d.tls === 'acme' ? 'on' : ''}"
                        onclick={() => toggleTls(d)}
                        disabled={toggling}
                        aria-label="Toggle HTTPS mode"
                        title={d.tls === 'acme' ? 'Automatic HTTPS · click for self-signed' : 'Self-signed · click for automatic HTTPS'}
                      >
                        <span class="tls-dot"></span>
                      </button>
                    {/if}
                    {#if d.kind === 'custom' && !isTenantZero}
                      <button type="button" class="dom-remove" onclick={() => removeHost(d)} disabled={removing} aria-label="Remove domain" title="Remove domain">
                        <Icon name="x" size={13} />
                      </button>
                    {/if}
                  </div>
                  {#if d.kind === 'custom' && d.dns && !d.dns.ok && !isLocalHost(d.host)}
                    <div class="dns-hint mono">
                      Point an A record for <b>{d.host}</b> to <b>{d.dns.expected_ip}</b>
                      {#if d.dns.resolves_to?.length}· currently {Array.isArray(d.dns.resolves_to) ? d.dns.resolves_to.join(', ') : d.dns.resolves_to}{/if}
                    </div>
                  {/if}
                  {#if d.kind === 'native' && isLocalHost(d.host)}
                    <div class="dns-hint mono">local native domain · no public DNS required</div>
                  {/if}
                  {#if d.status === 'failed' && d.tls === 'acme'}
                    <div class="dns-hint mono fail">
                      {d.error ?? 'Certificate issuance failed.'}
                      {#if d.next_retry_unix}· next automatic retry {relativeTime(d.next_retry_unix * 1000)}{/if}
                      <button type="button" class="linklike retry-acme" onclick={() => retryAcme(d)} disabled={retrying}>
                        {retrying ? 'retrying…' : 'retry now'}
                      </button>
                    </div>
                  {/if}
                </div>
              {/each}
            </div>
          {:else if isTenantZero}
            <div class="empty mono">
              no dashboard domain set ·
              <button class="linklike" onclick={() => go('settings')}>set one in Settings</button>
            </div>
          {:else if domains}
            <div class="empty mono">no custom domains · add one above</div>
          {:else}
            <div class="empty mono">loading domains…</div>
          {/if}
        </section>

        <!-- ————— the cage ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Cage</span>
            {#if app.lifecycle_state === 'ready'}
              <span class="pill live">ready</span>
            {:else if app.lifecycle_state === 'building'}
              <span class="pill build">swapping</span>
            {:else}
              <span class="pill ghost">cold</span>
            {/if}
          </div>
          {#if app.lifecycle_state === 'cold'}
            <div class="coldbox">
              <p>No process. The artifact sleeps on disk — {app.env_keys?.length ?? 0} env keys sealed, route armed.</p>
              <div class="coldstat num">next request revives the cage</div>
            </div>
          {:else}
            <div class="kv">
              <div class="kvrow"><span>Request p50</span><b class="num">{am && am.requests_1h ? millis(am.p50_ms) : '—'}</b></div>
              <div class="kvrow"><span>Request p99</span><b class="num">{am && am.requests_1h ? millis(am.p99_ms) : '—'}</b></div>
              <div class="kvrow"><span>rps</span><b class="num">{am ? rate(am.rps_1m) : '0'}</b></div>
            </div>
            {#if store.metrics?.totals?.boot_p50_ms}
              <div class="kv" style="margin-top:8px;padding-top:8px;border-top:1px solid var(--line-2)">
                <div class="kvrow"><span>Revival p50</span><b class="num">{millis(store.metrics.totals.boot_p50_ms)}</b></div>
                <div class="kvrow"><span>Revival p99</span><b class="num">{millis(store.metrics.totals.boot_p99_ms)}</b></div>
              </div>
            {/if}
          {/if}
        </section>

        <!-- ————— controls (read-only facts) ————— -->
        <section class="card">
          <div class="cardhead"><span class="label">Controls</span></div>
          <div class="kv">
            <div class="kvrow"><span>Pinned</span><span class="factpill {app.pinned ? 'on' : ''}">{app.pinned ? 'pinned warm' : 'unpinned'}</span></div>
            <div class="kvrow"><span>Egress</span><b class="num">{app.egress ?? '—'}</b></div>
            <div class="kvrow"><span>Idle TTL</span><b class="num">{fmtIdle(app.idle_ttl_ms)}</b></div>
            <div class="kvrow"><span>Memory cap</span><b class="num">{app.memory_max ? bytes(app.memory_max) : '—'}</b></div>
          </div>
        </section>

        <!-- ————— environment variables ————— -->
        <section class="card">
          <div class="cardhead">
            <span class="label">Environment</span>
            <span class="envcount num">{envVarEntries.length} vars</span>
          </div>
          <form class="env-add" onsubmit={submitEnvVar}>
            <input bind:value={newEnvKey} placeholder="KEY" maxlength="128" autocomplete="off" spellcheck="false" required />
            <input bind:value={newEnvValue} placeholder="value" maxlength="4096" autocomplete="off" spellcheck="false" required />
            <button class="btn cobalt sm" type="submit" disabled={envVarBusy || !newEnvKey.trim() || !newEnvValue}>
              {envVarBusy ? 'Saving…' : 'Set'}
            </button>
          </form>
          {#if envVarError}<p class="dom-err" role="alert">{envVarError}</p>{/if}
          {#if envVarEntries.length}
            <div class="kv">
              {#each envVarEntries as [key, value] (key)}
                <div class="kvrow env">
                  <span class="num">{key}</span>
                  <span class="env-value num">{envValueVisible[key] ? value : '••••••••'}</span>
                  <button
                    type="button"
                    class="btn icon sm"
                    onclick={() => (envValueVisible = { ...envValueVisible, [key]: !envValueVisible[key] })}
                    aria-label={envValueVisible[key] ? `Hide ${key}` : `Reveal ${key}`}
                  >
                    <Icon name="eye" size={12} />
                  </button>
                  <button
                    type="button"
                    class="btn icon sm"
                    onclick={() => removeEnvVarRow(key)}
                    disabled={envVarPending[key]}
                    aria-label="Remove {key}"
                  >
                    <Icon name="x" size={12} />
                  </button>
                </div>
              {/each}
            </div>
          {:else}
            <div class="empty mono">no env vars set</div>
          {/if}
          <div class="sealed"><Icon name="lock" size={11} /> encrypted at rest · decrypted here only for this authenticated session</div>
        </section>
      </aside>
    </div>
  </div>

  {#if rollbackOpen}
    <div class="scrim" onclick={(e) => { if (e.target === e.currentTarget) rollbackOpen = false; }} role="presentation">
      <div class="dialog" role="dialog" aria-label="Confirm rollback">
        <p class="eyebrow">CONFIRM ROLLBACK</p>
        <h2>Swap {app.name} to a prior deployment?</h2>
        <p class="dcopy">The active artifact is checked (CAS) before the retained deployment is promoted. No rebuild is started.</p>
        {#if rollbackCandidates.length}
          <div class="targets">
            {#each rollbackCandidates as d (d.id)}
              <label class="target">
                <input type="radio" name="rb" value={d.id} bind:group={rollbackTarget} />
                <span class="led {LED[d.status]}"></span>
                <span class="tid num">{d.id}</span>
                <span class="tart num">{shortHash(d.artifact_hash)}</span>
                <span class="twhen num">{relativeTime(d.created_ms)}</span>
              </label>
            {/each}
          </div>
        {/if}
        {#if rollbackError}<p class="rerr" role="alert">{rollbackError}</p>{/if}
        <div class="dactions">
          <button class="btn" onclick={() => (rollbackOpen = false)}>Cancel</button>
          <button class="btn danger" onclick={confirmRollback} disabled={rollbackBusy || !rollbackTarget}>
            {rollbackBusy ? 'Rolling back…' : 'Confirm rollback'}
          </button>
        </div>
      </div>
    </div>
  {/if}
{:else}
  <div class="page screen-enter">
    <div class="empty mono">no app selected</div>
  </div>
{/if}

<style>
  .page {
    max-width: 1264px;
    margin: 0 auto;
    padding: 26px 44px 0;
  }

  .head {
    display: flex;
    align-items: center;
    gap: 16px;
    margin-bottom: 24px;
  }
  .title { flex: 1; min-width: 0; }
  .row1 {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  h1 {
    font-size: 23px;
    font-weight: 650;
    letter-spacing: -0.02em;
  }
  .domains {
    display: flex;
    align-items: center;
    gap: 8px;
    margin-top: 4px;
    font-size: 12px;
  }
  .dom {
    color: var(--ink-3);
    display: inline-flex;
    align-items: center;
    gap: 4px;
    transition: color 0.12s ease;
  }
  .dom:hover { color: var(--cobalt-deep); }
  .dot { color: var(--ink-4); }
  .actions { display: flex; gap: 9px; }

  .grid {
    display: grid;
    grid-template-columns: 1fr 322px;
    gap: 20px;
    align-items: start;
  }
  .main { display: flex; flex-direction: column; gap: 16px; }
  .side { display: flex; flex-direction: column; gap: 16px; }

  /* production card */
  .prod { padding: 20px 22px 16px; }
  .prodhead {
    display: flex;
    justify-content: space-between;
    align-items: center;
    margin-bottom: 14px;
  }
  .commit {
    font-size: 17px;
    font-weight: 650;
    letter-spacing: -0.015em;
    text-align: left;
    line-height: 1.35;
    transition: color 0.12s ease;
  }
  .commit:hover { color: var(--cobalt-deep); }
  .meta {
    margin: 7px 0 16px;
    font-size: 11.5px;
    color: var(--ink-3);
  }
  .prodfoot {
    display: flex;
    align-items: flex-end;
    gap: 28px;
    padding-top: 15px;
  }
  .stat { display: flex; flex-direction: column; gap: 6px; }
  .readout.md { font-size: 21px; line-height: 1; }
  .grow { flex: 1; }

  .cardhead {
    display: flex;
    align-items: center;
    justify-content: space-between;
    padding: 15px 18px 10px;
  }
  .histbars { opacity: 0.9; }

  .rows { padding: 4px 8px 8px; }
  .drow {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 13px;
    padding: 11px 10px;
    border-radius: 10px;
    text-align: left;
    transition: background 0.12s ease;
  }
  .drow:hover { background: var(--surface-2); }
  .drow + .drow { border-top: 1px solid var(--line-2); }
  .dcommit {
    flex: 1;
    min-width: 0;
    font-size: 13px;
    color: var(--ink);
    font-weight: 500;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .chip { color: var(--ink-3); max-width: 160px; overflow: hidden; }
  .dnum { font-size: 11px; color: var(--ink-3); width: 44px; text-align: right; flex: none; }
  .when { width: 52px; }
  .chev { color: var(--ink-4); }

  /* side cards */
  .kv { padding: 2px 10px 12px; }
  .kvrow {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 8.5px 8px;
    font-size: 12.5px;
  }
  .kvrow + .kvrow { border-top: 1px solid var(--line-2); }
  .kvrow span { color: var(--ink-3); }
  .kvrow b { color: var(--ink); font-weight: 500; font-size: 12px; }
  .kvrow.env {
    display: grid;
    grid-template-columns: 1fr 1fr auto auto;
    gap: 8px;
    align-items: center;
  }
  .kvrow.env span { font-size: 11px; color: var(--ink-2); }
  .env-value { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .factpill {
    font-family: var(--mono);
    font-size: 10.5px;
    padding: 2px 8px;
    border-radius: 7px;
    color: var(--ink-3);
    background: var(--surface-3);
    border: 1px solid var(--line-2);
  }
  .factpill.on { color: #087a45; background: var(--live-soft); border-color: transparent; }
  .badge {
    font-family: var(--mono);
    font-size: 9.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
    background: var(--surface-3);
    border-radius: 6px;
    padding: 2px 7px;
  }
  .empty {
    padding: 28px 18px;
    font-size: 11px;
    color: var(--ink-4);
    text-align: center;
    letter-spacing: 0.06em;
  }
  .commit.dim { color: var(--ink-3); font-size: 14px; }

  .coldbox { padding: 4px 18px 16px; }
  .coldbox p {
    font-size: 12.5px;
    color: var(--ink-2);
    line-height: 1.6;
  }
  .coldstat {
    margin: 12px 0;
    padding: 9px 12px;
    background: var(--cobalt-ghost);
    border-radius: 9px;
    font-size: 11.5px;
    color: var(--cobalt-deep);
  }

  .envcount { font-size: 11px; color: var(--ink-3); }
  .sealed {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 10px 18px 14px;
    font-size: 10.5px;
    font-family: var(--mono);
    color: var(--ink-4);
    border-top: 1px solid var(--line-2);
  }
  .env-add {
    display: grid;
    grid-template-columns: 1fr 1fr auto;
    gap: 9px;
    align-items: center;
    padding: 12px 16px 14px;
    border-bottom: 1px solid var(--line-2);
  }
  .env-add input {
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 11px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .env-add input:focus-visible { outline: 2px solid var(--cobalt); outline-offset: 1px; }

  /* domains card */
  .dom-add {
    display: grid;
    grid-template-columns: 1fr auto;
    gap: 9px;
    align-items: center;
    padding: 12px 16px 14px;
    border-bottom: 1px solid var(--line-2);
  }
  .dom-add input {
    border: 1px solid var(--line-strong);
    border-radius: 8px;
    background: var(--surface);
    color: var(--ink);
    padding: 9px 11px;
    font-family: var(--mono);
    font-size: 12px;
  }
  .dom-add input:focus-visible { outline: 2px solid var(--cobalt); outline-offset: 1px; }
  .dom-add .dom-err { grid-column: 1 / -1; color: var(--red); font-size: 11px; margin: 0; }
  .linklike {
    background: none;
    border: 0;
    padding: 0;
    color: var(--cobalt-deep);
    cursor: pointer;
    font: inherit;
    text-decoration: underline;
  }
  .linklike:hover { color: var(--cobalt); }

  .dom-list { padding: 4px 10px 6px; }
  .dom-row {
    display: grid;
    grid-template-columns: 1fr auto auto;
    column-gap: 10px;
    row-gap: 6px;
    align-items: center;
    padding: 10px 8px;
  }
  .dom-row + .dom-row { border-top: 1px solid var(--line-2); }
  .dom-host { display: flex; align-items: center; gap: 9px; min-width: 0; }
  .dh-link {
    font-family: var(--mono);
    font-size: 12.5px;
    font-weight: 500;
    color: var(--ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    transition: color 0.12s ease;
  }
  .dh-link:hover { color: var(--cobalt-deep); }
  .dom-tag {
    font-family: var(--mono);
    font-size: 9.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--ink-3);
    background: var(--surface-3);
    border: 1px solid var(--line-2);
    border-radius: 6px;
    padding: 2px 6px;
    flex: none;
  }
  .dom-tag.primary {
    color: var(--cobalt-deep);
    background: var(--cobalt-ghost);
    border-color: color-mix(in srgb, var(--cobalt) 25%, var(--line-2));
  }
  .dom-primary-set {
    font-family: var(--mono);
    font-size: 9.5px;
    letter-spacing: 0.04em;
    flex: none;
  }
  .retry-acme {
    font-family: var(--mono);
    font-size: 10.5px;
    margin-left: 6px;
  }
  .dom-actions { display: flex; align-items: center; gap: 6px; }
  .tls-toggle {
    width: 24px;
    height: 24px;
    border-radius: 7px;
    border: 1px solid var(--line);
    background: var(--surface);
    display: grid;
    place-items: center;
    transition: border-color 0.14s ease, background 0.14s ease;
  }
  .tls-toggle:hover { border-color: var(--ink-4); }
  .tls-toggle:disabled { opacity: 0.5; cursor: not-allowed; }
  .tls-dot {
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--ink-4);
    transition: background 0.14s ease, box-shadow 0.14s ease;
  }
  .tls-toggle.on { border-color: color-mix(in srgb, var(--cobalt) 35%, var(--line)); background: var(--cobalt-ghost); }
  .tls-toggle.on .tls-dot { background: var(--cobalt); box-shadow: 0 0 0 3px var(--cobalt-soft); }
  .dom-remove {
    width: 24px;
    height: 24px;
    border-radius: 7px;
    border: 1px solid transparent;
    background: transparent;
    color: var(--ink-4);
    display: grid;
    place-items: center;
    transition: color 0.14s ease, background 0.14s ease;
  }
  .dom-remove:hover { color: var(--red); background: var(--red-soft); }
  .dom-remove:disabled { opacity: 0.5; cursor: not-allowed; }

  .dns-hint {
    grid-column: 1 / -1;
    font-size: 10.5px;
    line-height: 1.55;
    color: var(--ink-3);
    letter-spacing: 0.01em;
    padding: 8px 10px;
    background: var(--surface-2);
    border: 1px solid var(--line-2);
    border-radius: 8px;
  }
  .dns-hint b { color: var(--ink); font-weight: 600; }
  .dns-hint.fail { color: #b02c23; background: var(--red-soft); border-color: color-mix(in srgb, var(--red) 25%, var(--line-2)); }

  @media (max-width: 1080px) {
    .grid { grid-template-columns: 1fr; }
  }

  /* rollback dialog */
  .scrim {
    position: fixed;
    inset: 0;
    z-index: 100;
    background: rgba(12, 15, 20, 0.3);
    backdrop-filter: blur(6px);
    display: flex;
    justify-content: center;
    align-items: flex-start;
    padding-top: 16vh;
  }
  .dialog {
    width: min(520px, calc(100vw - 32px));
    background: var(--surface);
    border: 1px solid var(--line);
    border-radius: 18px;
    box-shadow: var(--shadow-pop);
    padding: 24px 24px 20px;
  }
  .eyebrow {
    color: var(--amber);
    font: 600 10px var(--mono);
    letter-spacing: 0.15em;
    text-transform: uppercase;
  }
  .dialog h2 { margin-top: 8px; font-size: 17px; letter-spacing: -0.015em; }
  .dcopy { margin-top: 8px; font-size: 12.5px; color: var(--ink-3); line-height: 1.55; }
  .targets { margin-top: 16px; display: flex; flex-direction: column; gap: 6px; max-height: 200px; overflow-y: auto; }
  .target {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 10px 12px;
    border: 1px solid var(--line-2);
    border-radius: 10px;
    cursor: pointer;
    font-size: 12px;
  }
  .target:hover { background: var(--surface-2); }
  .target input { accent-color: var(--cobalt); }
  .target .tid { color: var(--ink); }
  .target .tart { color: var(--ink-3); margin-left: auto; }
  .target .twhen { color: var(--ink-4); }
  .rerr { margin-top: 12px; color: var(--red); font-size: 11.5px; }
  .dactions { display: flex; justify-content: flex-end; gap: 10px; margin-top: 18px; }
  .btn.danger { color: var(--red); border-color: color-mix(in srgb, var(--red) 35%, var(--line)); }
  .btn.danger:hover { background: var(--red-soft); }
</style>
