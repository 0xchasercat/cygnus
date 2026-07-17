// The LiveStore — one reactive source of truth for the console.
// Svelte 5 runes ($state class). When a daemon is present it polls the live
// API; when not, it seeds itself from fixtures and stops. Screens read one
// shape either way. Each endpoint fetch is independent: a 404 on the parallel
// metrics/events/requests branch never poisons status/apps/deployments.

import { api, post, ApiError } from './api.js';
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
const GITHUB_EVERY = 3; // every 3rd tick

class Store {
  mode = $state('loading'); // 'loading' | 'live' | 'preview'
  auth = $state('unknown'); // 'unknown' | 'locked' | 'signin' | 'ready'
  node = $state(null);
  apps = $state([]);
  deployments = $state([]);
  metrics = $state(null);
  events = $state([]);
  requests = $state([]);
  github = $state({ configured: false, app: null, repositories: [], jobs: [] });
  connected = $state(true);
  lastSync = $state(0);
  notice = $state(''); // transient GitHub callback / mutation notice
  buildLogByDeploy = $state({}); // deploymentId -> string[] (preview fallback)

  #timer = null;
  #tick = 0;
  #booted = false;

  async boot() {
    if (this.#booted) return;
    this.#booted = true;
    try {
      const health = await api('/healthz');
      if (health?.mode !== 'live') {
        this.seedPreview();
        return;
      }
      this.mode = 'live';
      await this.#resolveAuth();
      this.#handleGithubCallback();
    } catch {
      // health unreachable — degrade to preview rather than hang.
      this.seedPreview();
    }
  }

  seedPreview() {
    this.mode = 'preview';
    this.auth = 'ready';
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

  #handleGithubCallback() {
    const params = new URLSearchParams(window.location.search);
    const g = params.get('github');
    const installationId = params.get('installation_id');
    if (g === 'configured') {
      this.notice = 'GitHub App created. Install it, then return to choose repositories.';
    } else if (g === 'setup' && /^\d+$/.test(installationId ?? '') && Number(installationId) > 0) {
      this.notice = 'GitHub App installed. Reading the selected repositories…';
    }
    if (g) window.history.replaceState({}, '', window.location.pathname);
  }

  // ——— polling ———
  start() {
    if (this.mode !== 'live' || this.auth !== 'ready') return;
    if (this.#timer) return;
    this.#poll();
    this.#timer = setInterval(() => this.#poll(), POLL_MS);
  }

  stop() {
    if (this.#timer) clearInterval(this.#timer);
    this.#timer = null;
  }

  async #poll() {
    if (this.mode !== 'live' || this.auth !== 'ready') return;
    this.#tick += 1;
    const fetchGithub = this.#tick % GITHUB_EVERY === 0;

    const reads = [
      this.#safeGet('/api/v1/status', (d) => (this.node = d?.node ?? this.node)),
      this.#safeGet('/api/v1/apps?limit=50', (d) => (this.apps = Array.isArray(d?.apps) ? d.apps : [])),
      this.#safeGet('/api/v1/deployments?limit=50', (d) => (this.deployments = Array.isArray(d?.deployments) ? d.deployments : [])),
      this.#safeGet('/api/v1/metrics', (d) => (this.metrics = d ?? null), true),
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
  async signIn(token) {
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
    this.node = null;
    this.apps = [];
    this.deployments = [];
    this.metrics = null;
    this.events = [];
    this.requests = [];
    this.github = { configured: false, app: null, repositories: [], jobs: [] };
    return { ok: true };
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
  async deployUpload({ app, domain, engineVersion, entry, tarball, totalBytes, onProgress }) {
    const begin = {
      app,
      total_bytes: totalBytes,
      ...(domain ? { domain } : {}),
      ...(engineVersion ? { engine_version: engineVersion } : {}),
      ...(entry ? { entry } : {}),
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
