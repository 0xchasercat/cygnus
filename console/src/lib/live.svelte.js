// The LiveStore — one reactive source of truth for the console.
// Svelte 5 runes ($state class). When a daemon is present it polls the live
// API; when not, it seeds itself from fixtures and stops. Screens read one
// shape either way. Each endpoint fetch is independent: a 404 on the parallel
// metrics/events/requests branch never poisons status/apps/deployments.

import { api, post, ApiError } from './api.js';
import { go, ui } from './stores.svelte.js';
import {
  previewNode,
  previewApps,
  previewDeployments,
  previewMetrics,
  previewEvents,
  previewRequests,
  previewGithub,
  previewBuildLog,
} from './fixtures.js';

const POLL_MS = 4000;
const BUILD_POLL_MS = 1000; // faster while any deploy is building
const GITHUB_EVERY = 3; // every 3rd tick
const DOMAIN_POLL_MS = 5000; // per-app domain status while non-terminal

class Store {
  mode = $state('loading'); // 'loading' | 'live' | 'preview'
  auth = $state('unknown'); // 'unknown' | 'locked' | 'setup' | 'signin' | 'ready'
  node = $state(null);
  apps = $state([]);
  deployments = $state([]);
  metrics = $state(null);
  events = $state([]);
  github = $state({ configured: false, app: null, repositories: [], jobs: [], discoverable: [], installations: [] });
  connected = $state(true);
  lastSync = $state(0);
  notice = $state(''); // transient GitHub callback / mutation notice
  buildLogByDeploy = $state({}); // deploymentId -> string[] (preview fallback)
  domainsByApp = $state({}); // appName -> {domains:[...], at:number} (live cache)
  envVarsByApp = $state({}); // appName -> {key: value} (live cache)
  #domainTimer = null; // polls per-app domain status while non-terminal

  #timer = null;
  #deployTimer = null;
  #tick = 0;
  #booted = false;
  #domainPollApp = null;

  async boot() {
    if (this.#booted) return;
    this.#booted = true;
    try {
      // /healthz is unwrapped — a flat {ok, service, mode, ...} object, NOT the
      // {ok, data} envelope the /api/v1/* routes use — so fetch it raw rather
      // than through api() (which would return envelope.data === undefined).
      const response = await fetch('/healthz', {
        headers: { accept: 'application/json' },
        credentials: 'same-origin',
      });
      const health = await response.json();
      if (health?.mode !== 'live') {
        this.seedPreview();
        return;
      }
      this.mode = 'live';
      // First-run: no admin account yet. Surface the setup wizard instead of
      // the login screen. setupRequired may be absent on older daemons — fall
      // through to the normal session resolve so auth degrades gracefully.
      if (health?.setupRequired === true) {
        this.auth = 'setup';
        return;
      }
      await this.#resolveAuth();
      await this.#handleGithubCallback();
    } catch {
      // health unreachable — degrade to preview rather than hang.
      this.seedPreview();
    }
  }

  seedPreview() {
    this.mode = 'preview';
    // ?setup=1 dev hook lets reviewers see the first-run wizard in preview.
    const forceSetup = typeof URLSearchParams !== 'undefined'
      && new URLSearchParams(window.location.search).get('setup') === '1';
    if (forceSetup) {
      this.auth = 'setup';
    } else {
      this.auth = 'ready';
    }
    this.node = previewNode;
    this.apps = previewApps;
    this.deployments = previewDeployments;
    this.metrics = previewMetrics;
    this.events = previewEvents;
    this.requests = previewRequests;
    this.github = previewGithub;
    this.connected = true;
    this.lastSync = Date.now();
    this.buildLogByDeploy = { dpl_9f2c1a4: previewBuildLog };
  }

  async #resolveAuth() {
    try {
      const session = await api('/api/v1/session');
      if (session?.setupRequired === true) {
        this.auth = 'setup';
        return;
      }
      if (session?.locked || !session?.configured) {
        this.auth = 'locked';
        return;
      }
      if (!session?.authenticated) {
        this.auth = 'signin';
        return;
      }
      this.auth = 'ready';
      this.start();
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) {
        this.auth = 'signin';
        return;
      }
      // Session endpoint down — treat as locked so the operator sees a path.
      this.auth = 'locked';
    }
  }

  async #handleGithubCallback() {
    const params = new URLSearchParams(window.location.search);
    const g = params.get('github');
    const installationId = params.get('installation_id');
    const hasInstallation = g === 'setup' && /^\d+$/.test(installationId ?? '') && Number(installationId) > 0;
    if (g === 'configured') {
      this.notice = 'GitHub App created. Install it on your account to continue.';
    } else if (hasInstallation) {
      this.notice = 'GitHub App installed. Discovering repositories…';
    }
    if (!g) return;
    window.history.replaceState({}, '', window.location.pathname);
    // Await so store.github.configured is populated before Settings renders.
    await this.refreshGithub();
    // Always auto-discover after any GitHub callback — no installation ID UI.
    await this.discoverRepositories();
    if (g === 'configured' || hasInstallation) go('settings');
  }

  // ——— polling ———
  start() {
    if (this.mode !== 'live' || this.auth !== 'ready') return;
    if (this.#timer) return;
    this.#poll();
    this.#pollDeployments();
    this.#timer = setInterval(() => this.#poll(), POLL_MS);
    this.#armDeployTimer();
  }

  // Deployments refresh on their own independent, faster-while-building
  // timer so a build page updates status within ~1s without forcing the
  // heavier apps/metrics/events/requests poll (and everything that renders
  // off it — sidebars, counts) to also re-fetch and re-render every tick.
  // That shared-poll churn was the actual source of the build page's
  // per-update flicker.
  #armDeployTimer() {
    clearInterval(this.#deployTimer);
    if (this.mode !== 'live' || this.auth !== 'ready') {
      this.#deployTimer = null;
      return;
    }
    const building = this.deployments.some((d) => d.status === 'building');
    this.#deployTimer = setInterval(
      () => this.#pollDeployments(),
      building ? BUILD_POLL_MS : POLL_MS,
    );
  }

  stop() {
    clearInterval(this.#timer);
    this.#timer = null;
    clearInterval(this.#deployTimer);
    this.#deployTimer = null;
  }

  async refreshGithub() {
    if (this.mode !== 'live' || this.auth !== 'ready') return;
    await Promise.allSettled([
      this.#safeGet('/api/v1/github/status', (d) => {
        this.github = { ...this.github, configured: d?.configured === true, app: d?.app ?? null };
      }),
      this.#safeGet('/api/v1/github/repositories?limit=50', (d) => {
        this.github = { ...this.github, repositories: Array.isArray(d?.repositories) ? d.repositories : [] };
      }),
      this.#safeGet('/api/v1/github/jobs?limit=50', (d) => {
        this.github = { ...this.github, jobs: Array.isArray(d?.jobs) ? d.jobs : [] };
      }),
    ]);
    // Discovery is explicit (callback / Settings / ShipModal) so we don't
    // burn GitHub rate limits on every status poll.
  }

  async #poll(forceGithub = false) {
    if (this.mode !== 'live' || this.auth !== 'ready') return;
    this.#tick += 1;
    const fetchGithub = forceGithub || (this.#tick % GITHUB_EVERY === 0);
    const reads = [
      this.#safeGet('/api/v1/status', (d) => (this.node = d?.node ?? this.node)),
      this.#safeGet('/api/v1/apps?limit=50', (d) => (this.apps = Array.isArray(d?.apps) ? d.apps : [])),
      this.#safeGet('/api/v1/metrics', (d) => {
        if (!d) {
          this.metrics = null;
          return;
        }
        // AdminData::Metrics is tagged {kind:"metrics", ...snapshot fields}.
        // Flatten so screens can read totals/series/apps directly.
        const { kind: _kind, ...snapshot } = d;
        this.metrics = Object.keys(snapshot).length ? snapshot : d;
      }, true),
      this.#safeGet('/api/v1/events?limit=100', (d) => (this.events = Array.isArray(d?.events) ? d.events : []), true),
      this.#safeGet('/api/v1/requests?limit=200', (d) => (this.requests = Array.isArray(d?.requests) ? d.requests.slice(0, 200) : []), true),
    ];
    if (fetchGithub) {
      reads.push(
        this.#safeGet('/api/v1/github/status', (d) => {
          this.github = { ...this.github, configured: d?.configured === true, app: d?.app ?? null };
        }),
        this.#safeGet('/api/v1/github/repositories?limit=50', (d) => {
          this.github = { ...this.github, repositories: Array.isArray(d?.repositories) ? d.repositories : [] };
        }),
        this.#safeGet('/api/v1/github/jobs?limit=50', (d) => {
          this.github = { ...this.github, jobs: Array.isArray(d?.jobs) ? d.jobs : [] };
        }),
      );
    }

    await Promise.all(reads);
    this.lastSync = Date.now();
  }

  // Replace this.deployments only when the payload actually differs (by id +
  // status + updated_ms), so an unchanged poll never hands the UI a fresh
  // array reference — that reference change was the real source of the
  // build page's per-tick flicker (every derived view recomputing off a
  // "new" object with identical fields).
  async #pollDeployments() {
    await this.#safeGet('/api/v1/deployments?limit=50', (d) => {
      const next = Array.isArray(d?.deployments) ? d.deployments : [];
      const changed =
        next.length !== this.deployments.length ||
        next.some((item, i) => {
          const prev = this.deployments[i];
          return (
            !prev ||
            prev.id !== item.id ||
            prev.status !== item.status ||
            prev.updated_ms !== item.updated_ms ||
            prev.artifact_hash !== item.artifact_hash ||
            prev.error !== item.error
          );
        });
      if (changed) this.deployments = next;
    });
    // Cadence depends on build activity, so re-arm every tick even when the
    // array reference didn't change.
    this.#armDeployTimer();
  }

  // Fetch one endpoint; 404 (parallel branch not merged yet) is silent.
  // 401 anywhere flips auth to signin. Network failure marks disconnected
  // but never blanks data. Returns true on success.
  async #safeGet(path, apply, tolerateMissing = false) {
    try {
      const data = await api(path);
      this.connected = true;
      apply(data);
      return true;
    } catch (cause) {
      if (cause instanceof ApiError) {
        if (cause.status === 401) {
          this.auth = 'signin';
          this.stop();
          return false;
        }
        if (tolerateMissing && (cause.status === 404 || cause.status === 405)) {
          // metrics/events/requests land on a parallel backend branch.
          return false;
        }
        // Other API errors (500s etc.) — keep last data, stay connected-ish.
        this.connected = true;
        return false;
      }
      // Network failure — keep last data, mark disconnected, retry next tick.
      this.connected = false;
      return false;
    }
  }

  // ——— mutations ———
  // First-run setup: create the admin account, store the dashboard/apex
  // domains + SSL baseline, and log in. The backend sets the session cookie.
  // 409 means an admin already exists — fall back to the login screen.
  async setup({ email, password, dashboardDomain, apexDomain, ssl }) {
    try {
      await post('/api/v1/setup', {
        email,
        password,
        dashboard_domain: dashboardDomain || '',
        apex_domain: apexDomain || '',
        ssl,
      });
      this.auth = 'ready';
      this.start();
      return { ok: true };
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 409) {
        this.auth = 'signin';
        return { ok: false, error: 'already_setup', status: 409 };
      }
      const msg = cause instanceof Error ? cause.message : 'Setup failed';
      return { ok: false, error: msg };
    }
  }

  async signIn({ email, password }) {
    try {
      const session = await post('/api/v1/session', { email, password });
      if (!session?.authenticated) {
        return { ok: false, error: 'Authentication did not complete.' };
      }
      this.auth = 'ready';
      this.start();
      return { ok: true };
    } catch (cause) {
      if (cause instanceof ApiError) {
        if (cause.status === 401) return { ok: false, error: 'Invalid email or password' };
        // 429 already shaped as "Too many attempts — retry in Ns" by api.js.
        if (cause.status === 429) return { ok: false, error: cause.message };
      }
      const msg = cause instanceof Error ? cause.message : 'Sign-in failed';
      return { ok: false, error: msg };
    }
  }

  // Recovery affordance: the backend keeps bootstrap-token login as a
  // fallback so a locked-out admin can re-enter with the installer token.
  async signInWithToken(token) {
    try {
      const session = await post('/api/v1/session', { token });
      if (!session?.authenticated) {
        return { ok: false, error: 'Authentication did not complete.' };
      }
      this.auth = 'ready';
      this.start();
      return { ok: true };
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) return { ok: false, error: 'Invalid token' };
      const msg = cause instanceof Error ? cause.message : 'Sign-in failed';
      return { ok: false, error: msg };
    }
  }

  async signOut() {
    try {
      await post('/api/v1/logout');
    } catch {
      /* best effort */
    }
    this.auth = 'signin';
    this.stop();
    this.#stopDomainPoll();
    this.node = null;
    this.apps = [];
    this.deployments = [];
    this.metrics = null;
    this.events = [];
    this.requests = [];
    this.github = { configured: false, app: null, repositories: [], jobs: [], discoverable: [], installations: [] };
    this.domainsByApp = {};
    this.envVarsByApp = {};
    return { ok: true };
  }

  // ——— per-app domains ———
  // Fetch + cache a domain list for an app. The endpoint may 404 on a
  // backend branch that hasn't landed yet — degrade to [] like metrics do.
  appDomains(appName) {
    const entry = this.domainsByApp[appName];
    return entry?.domains ?? null;
  }

  async refreshAppDomains(appName) {
    if (!appName) return null;
    try {
      const data = await api(`/api/v1/apps/${encodeURIComponent(appName)}/domains`);
      const domains = Array.isArray(data?.domains) ? data.domains : [];
      this.domainsByApp = { ...this.domainsByApp, [appName]: { domains, at: Date.now() } };
      this.#maybePollDomains(appName);
      return domains;
    } catch (cause) {
      if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
        // Branch not merged yet — keep last cache (or empty) and stay quiet.
        if (!this.domainsByApp[appName]) {
          this.domainsByApp = { ...this.domainsByApp, [appName]: { domains: [], at: Date.now() } };
        }
        return this.domainsByApp[appName].domains;
      }
      if (cause instanceof ApiError && cause.status === 401) {
        this.auth = 'signin';
        this.stop();
      }
      return this.domainsByApp[appName]?.domains ?? null;
    }
  }

  async addDomain(appName, host) {
    try {
      await post(`/api/v1/apps/${encodeURIComponent(appName)}/domains`, { host });
      this.notice = `Domain ${host} added for ${appName}.`;
      await this.refreshAppDomains(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not add domain' };
    }
  }

  async removeDomain(appName, host) {
    try {
      await api(`/api/v1/apps/${encodeURIComponent(appName)}/domains/${encodeURIComponent(host)}`, {
        method: 'DELETE',
      });
      this.notice = `Domain ${host} removed from ${appName}.`;
      await this.refreshAppDomains(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not remove domain' };
    }
  }

  // ——— per-app environment variables ———
  envVars(appName) {
    return this.envVarsByApp[appName] ?? null;
  }

  async refreshEnvVars(appName) {
    if (!appName) return null;
    try {
      const data = await api(`/api/v1/apps/${encodeURIComponent(appName)}/env`);
      const vars = data?.vars && typeof data.vars === 'object' ? data.vars : {};
      this.envVarsByApp = { ...this.envVarsByApp, [appName]: vars };
      return vars;
    } catch (cause) {
      if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
        if (!this.envVarsByApp[appName]) {
          this.envVarsByApp = { ...this.envVarsByApp, [appName]: {} };
        }
        return this.envVarsByApp[appName];
      }
      if (cause instanceof ApiError && cause.status === 401) {
        this.auth = 'signin';
        this.stop();
      }
      return this.envVarsByApp[appName] ?? null;
    }
  }

  async setEnvVar(appName, key, value) {
    try {
      await post(`/api/v1/apps/${encodeURIComponent(appName)}/env`, { key, value });
      this.notice = `${key} set for ${appName}.`;
      await this.refreshEnvVars(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not set env var' };
    }
  }

  async removeEnvVar(appName, key) {
    try {
      await api(`/api/v1/apps/${encodeURIComponent(appName)}/env/${encodeURIComponent(key)}`, {
        method: 'DELETE',
      });
      this.notice = `${key} removed from ${appName}.`;
      await this.refreshEnvVars(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not remove env var' };
    }
  }

  async setDomainTls(appName, host, mode) {
    try {
      await post(
        `/api/v1/apps/${encodeURIComponent(appName)}/domains/${encodeURIComponent(host)}/tls`,
        { mode },
      );
      this.notice = `TLS set to ${mode === 'acme' ? 'automatic' : 'self-signed'} for ${host}.`;
      await this.refreshAppDomains(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not change TLS' };
    }
  }

  async setPrimaryDomain(appName, host) {
    try {
      await post(`/api/v1/apps/${encodeURIComponent(appName)}/domains/${encodeURIComponent(host)}/primary`);
      this.notice = `${host} is now the primary domain for ${appName}.`;
      await this.refreshAppDomains(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not set primary domain' };
    }
  }

  async retryDomainAcme(appName, host) {
    try {
      await post(`/api/v1/apps/${encodeURIComponent(appName)}/domains/${encodeURIComponent(host)}/retry-acme`);
      this.notice = `Retrying certificate issuance for ${host}.`;
      await this.refreshAppDomains(appName);
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not retry certificate issuance' };
    }
  }

  // ——— dashboard domain + SSL (settings) ———
  async setDashboardDomain(domain, apex) {
    try {
      await post('/api/v1/settings/dashboard-domain', { domain, apex });
      this.notice = 'Dashboard domain updated.';
      await this.#safeGet('/api/v1/status', (d) => (this.node = d?.node ?? this.node));
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not update dashboard domain' };
    }
  }

  async setDashboardTls(mode, email) {
    try {
      const body = { mode };
      if (email) body.email = email;
      await post('/api/v1/settings/dashboard-tls', body);
      this.notice = `Dashboard TLS set to ${mode === 'acme' ? 'automatic' : 'self-signed'}.`;
      await this.#safeGet('/api/v1/status', (d) => (this.node = d?.node ?? this.node));
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not change dashboard TLS' };
    }
  }

  async changePassword({ email, currentPassword, newPassword }) {
    try {
      await post('/api/v1/settings/password', {
        email,
        current_password: currentPassword,
        new_password: newPassword,
      });
      this.notice = 'Password changed.';
      return { ok: true };
    } catch (cause) {
      if (cause instanceof ApiError && cause.status === 401) {
        return { ok: false, error: 'Current password is incorrect' };
      }
      return { ok: false, error: cause instanceof Error ? cause.message : 'Could not change password' };
    }
  }

  // Poll per-app domain status every ~5s while any domain is non-terminal
  // (issuing / pending / fallback_active), so pills update live as DNS
  // propagates and certs issue. Stops once everything reaches a terminal
  // state or the app changes.
  #maybePollDomains(appName) {
    const entry = this.domainsByApp[appName];
    if (!entry) return;
    const pending = entry.domains.some((d) =>
      d.status === 'issuing' || d.status === 'pending' || d.status === 'fallback_active'
    );
    if (pending) {
      this.#startDomainPoll(appName);
    } else {
      this.#stopDomainPoll();
    }
  }

  #startDomainPoll(appName) {
    this.#domainPollApp = appName;
    if (this.#domainTimer) return;
    this.#domainTimer = setInterval(() => {
      if (this.mode !== 'live' || this.auth !== 'ready') {
        this.#stopDomainPoll();
        return;
      }
      const app = this.#domainPollApp;
      if (app) this.refreshAppDomains(app);
    }, DOMAIN_POLL_MS);
  }

  #stopDomainPoll() {
    if (this.#domainTimer) clearInterval(this.#domainTimer);
    this.#domainTimer = null;
    this.#domainPollApp = null;
  }

  async mapDomain(app, domain) {
    try {
      await post('/api/v1/map-domain', { app, domain });
      this.notice = `Domain mapped for ${app}.`;
      await this.#poll();
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Domain mapping failed' };
    }
  }

  async rollback(app, deployment, expectedArtifact) {
    if (!expectedArtifact) {
      return { ok: false, error: 'The active artifact is unavailable; refresh before rolling back.' };
    }
    try {
      await post('/api/v1/rollback', {
        app,
        deployment,
        expected_active_artifact: expectedArtifact,
      });
      this.notice = `Rollback submitted for ${app}.`;
      await this.#poll();
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Rollback failed' };
    }
  }

  async githubManifest(owner) {
    try {
      const body = owner ? { owner } : {};
      const result = await post('/api/v1/github/manifest', body);
      if (!result?.action || !result?.manifest) {
        return { ok: false, error: 'GitHub setup link was incomplete' };
      }
      // Build a form off-screen rather than `form.hidden = true` —
      // `display: none` (which the `hidden` attribute applies) can prevent
      // `form.submit()` from navigating in some browsers, and the form
      // submission is what carries the manifest JSON to github.com.
      const form = document.createElement('form');
      form.method = 'POST';
      form.action = result.action;
      form.style.position = 'absolute';
      form.style.left = '-9999px';
      form.style.top = '0';
      form.setAttribute('aria-hidden', 'true');
      const input = document.createElement('input');
      input.type = 'hidden';
      input.name = 'manifest';
      input.value = JSON.stringify(result.manifest);
      form.append(input);
      document.body.append(form);
      // Submit synchronously. If the call throws (popup blocker, CSP),
      // fall back to opening the manifest creation page in a new tab and
      // surface a clear error so the operator knows the form auto-submit
      // failed instead of seeing a silent "nothing happened".
      try {
        form.submit();
      } catch (submitError) {
        try { document.body.removeChild(form); } catch { /* ignore */ }
        return {
          ok: false,
          error: 'Browser blocked the form auto-submit to GitHub; open the page manually and paste the manifest if this persists.',
        };
      }
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'GitHub setup could not start' };
    }
  }

  async listInstallationRepositories(installationId) {
    try {
      const result = await api(`/api/v1/github/installations/${encodeURIComponent(installationId)}/repositories`);
      return { ok: true, repositories: Array.isArray(result?.repositories) ? result.repositories : [] };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Repository discovery failed', repositories: [] };
    }
  }

  // Preferred discovery path: list every installation + accessible repo via
  // the GitHub App JWT. No operator-supplied installation ID required.
  async discoverRepositories() {
    if (this.mode !== 'live' || this.auth !== 'ready') {
      return { ok: false, error: 'Not ready', repositories: [], installations: [] };
    }
    try {
      const result = await api('/api/v1/github/discoverable-repositories');
      const repositories = Array.isArray(result?.repositories) ? result.repositories : [];
      const installations = Array.isArray(result?.installations) ? result.installations : [];
      this.github = { ...this.github, discoverable: repositories, installations };
      return { ok: true, repositories, installations };
    } catch (cause) {
      return {
        ok: false,
        error: cause instanceof Error ? cause.message : 'Repository discovery failed',
        repositories: [],
        installations: [],
      };
    }
  }

  async configureRepository(cfg) {
    try {
      await post('/api/v1/github/repositories', cfg);
      this.notice = `Mapped ${cfg.owner}/${cfg.name} to Tenant Zero.`;
      await this.#poll();
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Repository configuration failed' };
    }
  }

  async retryJob(id) {
    try {
      await post(`/api/v1/github/jobs/${encodeURIComponent(id)}/retry`);
      this.notice = `Retry queued.`;
      await this.#poll();
      return { ok: true };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'Retry could not be queued' };
    }
  }

  // ——— deploy upload (chunked) ———
  // Packs an already-built tarball (Uint8Array) through begin/chunk/finish,
  // reporting progress via the onProgress callback (0..1). Returns the
  // deployment_id on success.
  async deployUpload({ app, domain, engineVersion, entry, env, preview, tarball, totalBytes, onProgress }) {
    const begin = {
      app,
      total_bytes: totalBytes,
      ...(domain ? { domain } : {}),
      ...(engineVersion ? { engine_version: engineVersion } : {}),
      ...(entry ? { entry } : {}),
      ...(env && Object.keys(env).length ? { env } : {}),
      ...(preview ? { preview } : {}),
    };
    let uploadId;
    try {
      const r = await post('/api/v1/deploy/begin', begin);
      uploadId = r?.upload_id;
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'deploy/begin failed' };
    }
    if (!uploadId) return { ok: false, error: 'deploy/begin returned no upload id' };

    const CHUNK = 1024 * 1024; // 1 MiB base64 per request
    let sent = 0;
    for (let off = 0; off < tarball.length; off += CHUNK) {
      const slice = tarball.subarray(off, off + CHUNK);
      const b64 = base64FromBytes(slice);
      try {
        const r = await post('/api/v1/deploy/chunk', { upload_id: uploadId, chunk_base64: b64 });
        sent = r?.received_bytes ?? sent + slice.length;
        onProgress?.(Math.min(1, sent / totalBytes));
      } catch (cause) {
        return { ok: false, error: cause instanceof Error ? cause.message : 'deploy/chunk failed' };
      }
    }

    try {
      const r = await post('/api/v1/deploy/finish', { upload_id: uploadId });
      const deploymentId = r?.deployment_id;
      if (!deploymentId) return { ok: false, error: 'deploy/finish returned no deployment id' };
      onProgress?.(1);
      // Refresh deployments so the build page has the new row immediately.
      await this.#safeGet('/api/v1/deployments?limit=50', (d) => {
        this.deployments = Array.isArray(d?.deployments) ? d.deployments : this.deployments;
      });
      return { ok: true, deploymentId };
    } catch (cause) {
      return { ok: false, error: cause instanceof Error ? cause.message : 'deploy/finish failed' };
    }
  }

  // ——— logs ———
  // Poll deployment logs while building (and once when opened). Decodes
  // base64 and splits into lines. Returns the appended lines array (mutates
  // state live via the passed setter).
  async fetchDeploymentLog(deploymentId, stream, offset) {
    try {
      const data = await api(
        `/api/v1/deployments/${encodeURIComponent(deploymentId)}/logs?stream=${stream}&offset=${offset}`,
      );
      if (!data) return { lines: [], nextOffset: offset, eof: true };
      const text = decodeBase64(data.data_base64);
      const lines = text.length ? text.split('\n') : [];
      return {
        lines,
        nextOffset: data.next_offset ?? offset,
        eof: data.eof ?? true,
      };
    } catch (cause) {
      if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
        return { lines: [], nextOffset: offset, eof: true, missing: true };
      }
      return { lines: [], nextOffset: offset, eof: true };
    }
  }

  async fetchAppLog(app, stream, offset) {
    try {
      const data = await api(
        `/api/v1/apps/${encodeURIComponent(app)}/logs?stream=${stream}&offset=${offset}`,
      );
      if (!data) return { lines: [], nextOffset: offset, eof: true };
      const text = decodeBase64(data.data_base64);
      const lines = text.length ? text.split('\n') : [];
      return {
        lines,
        nextOffset: data.next_offset ?? offset,
        eof: data.eof ?? true,
      };
    } catch (cause) {
      if (cause instanceof ApiError && (cause.status === 404 || cause.status === 405)) {
        return { lines: [], nextOffset: offset, eof: true, missing: true };
      }
      return { lines: [], nextOffset: offset, eof: true };
    }
  }

  // ——— derived helpers (read-only views) ———
  appByName(name) {
    return this.apps.find((a) => a.name === name) ?? null;
  }

  deploymentsFor(appName) {
    return this.deployments.filter((d) => d.app === appName);
  }

  deploymentById(id) {
    return this.deployments.find((d) => d.id === id) ?? null;
  }

  appMetrics(appName) {
    if (!this.metrics?.apps) return null;
    return this.metrics.apps.find((a) => a.app === appName) ?? null;
  }

  // Per-app request spark, bucketed into minutes client-side.
  appRequestSeries(appName, buckets = 18) {
    if (!this.requests.length) return [];
    const now = Date.now();
    const width = 60_000;
    const counts = new Array(buckets).fill(0);
    for (const r of this.requests) {
      if (r.app !== appName) continue;
      const age = now - (r.time_ms ?? 0);
      if (age < 0 || age > buckets * width) continue;
      const idx = buckets - 1 - Math.floor(age / width);
      if (idx >= 0 && idx < buckets) counts[idx] += 1;
    }
    return counts;
  }
}

function base64FromBytes(bytes) {
  let s = '';
  for (let i = 0; i < bytes.length; i++) s += String.fromCharCode(bytes[i]);
  return btoa(s);
}

function decodeBase64(b64) {
  if (!b64) return '';
  try {
    const bin = atob(b64);
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return new TextDecoder().decode(bytes);
  } catch {
    return '';
  }
}

export const store = new Store();
