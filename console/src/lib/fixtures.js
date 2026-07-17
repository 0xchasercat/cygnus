// Preview dataset — characterful public demo, reshaped to exactly the live
// API contract so the UI has one code path. When a daemon is present the
// LiveStore replaces these with real data; when not, the same screens render.

const now = Date.now();
const MIN = 60_000;

function ts(minAgo) {
  return now - minAgo * MIN;
}

export const previewNode = {
  listen: '0.0.0.0:3443',
  https_listen: '0.0.0.0:3443',
  apps_domain: 'swan.host',
  app_count: 8,
  version: 'cygnus 0.9.2',
  uptime_seconds: 41 * 86400 + 7 * 3600,
  isolation: 'userns mntns pidns netns · seccomp v14',
  warm_count: 5,
  engines: [
    { version: 'bun 1.2.19', sha256: '9f2c1a4b8e11d7a0c3f2e11a', default: true, apps: 6 },
    { version: 'bun 1.1.34', sha256: 'a1b2c3d4e5f6a7b8c9d0e1f2', default: false, apps: 1 },
  ],
  certificates: [
    { domain: '*.swan.host', kind: 'wildcard · DNS-01', ok: true, expires_unix: ts(60 * 24 * 61) / 1000 },
    { domain: 'atelier.studio', kind: 'custom · HTTP-01', ok: true, expires_unix: ts(60 * 24 * 34) / 1000 },
    { domain: 'helios.dev', kind: 'custom · HTTP-01', ok: true, expires_unix: ts(60 * 24 * 9) / 1000 },
  ],
  memory: {
    total_bytes: 32 * 1024 * 1024 * 1024,
    available_bytes: (32 - 7.3) * 1024 * 1024 * 1024,
  },
};

export const previewApps = [
  {
    name: 'atelier',
    domains: ['atelier.swan.host', 'atelier.studio'],
    lifecycle_state: 'ready',
    pinned: true,
    idle_ttl_ms: 0,
    egress: 'public',
    memory_max: 256 * 1024 * 1024,
    env_keys: ['DATABASE_URL', 'STRIPE_SECRET', 'SESSION_KEY', 'REDIS_URL'],
    active: {
      deployment_id: 'dpl_9f2c1a4',
      artifact_hash: '9f2c1a4b8e11d7a0c3f2e11ad7a0c3f2e11ab9e02f1',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'helios-api',
    domains: ['helios-api.swan.host', 'helios.dev'],
    lifecycle_state: 'ready',
    pinned: true,
    idle_ttl_ms: 0,
    egress: 'public',
    memory_max: 512 * 1024 * 1024,
    env_keys: ['DATABASE_URL', 'EVENTS_KEY', 'OTEL_EXPORTER'],
    active: {
      deployment_id: 'dpl_2ac401d',
      artifact_hash: '2ac401d7e3b9f1a8c4d2e6f0b1a3c5d7e9f1a2b3',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'ledger',
    domains: ['ledger.swan.host'],
    lifecycle_state: 'ready',
    pinned: false,
    idle_ttl_ms: 10 * MIN,
    egress: 'restricted',
    memory_max: 256 * 1024 * 1024,
    env_keys: ['DATABASE_URL', 'LEDGER_KEY'],
    active: {
      deployment_id: 'dpl_b7d0c11',
      artifact_hash: 'b7d0c11a3f9e2d8c1b4a6e0f2d7c9b1a3e5f7d9c',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'prism-docs',
    domains: ['prism-docs.swan.host'],
    lifecycle_state: 'cold',
    pinned: false,
    idle_ttl_ms: 10 * MIN,
    egress: 'public',
    memory_max: 256 * 1024 * 1024,
    env_keys: ['ALGOLIA_KEY'],
    active: {
      deployment_id: 'dpl_77c9d3b',
      artifact_hash: '77c9d3b1a2c4e6f8d0b2a4c6e8f0a2b4c6e8f0a2',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'pulse-widget',
    domains: ['pulse-widget.swan.host'],
    lifecycle_state: 'building',
    pinned: false,
    idle_ttl_ms: 10 * MIN,
    egress: 'none',
    memory_max: 128 * 1024 * 1024,
    env_keys: ['BEACON_KEY'],
    active: null,
  },
  {
    name: 'pr-412-checkout',
    domains: ['pr-412-checkout.swan.host'],
    lifecycle_state: 'cold',
    pinned: false,
    idle_ttl_ms: 10 * MIN,
    egress: 'public',
    memory_max: 256 * 1024 * 1024,
    env_keys: ['DATABASE_URL', 'STRIPE_SECRET'],
    active: {
      deployment_id: 'dpl_e33ab90',
      artifact_hash: 'e33ab901c2d4f6a8e0b2c4d6f8a0b2c4d6e8f0a2',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'pr-409-i18n',
    domains: ['pr-409-i18n.swan.host'],
    lifecycle_state: 'cold',
    pinned: false,
    idle_ttl_ms: 10 * MIN,
    egress: 'public',
    memory_max: 256 * 1024 * 1024,
    env_keys: ['DATABASE_URL'],
    active: {
      deployment_id: 'dpl_e90b774',
      artifact_hash: 'e90b7741a2c4e6f8a0b2c4d6e8f0a2b4c6e8f0a2',
      engine_version: 'bun 1.2.19',
    },
  },
  {
    name: 'orbit-cron',
    domains: ['orbit-cron.swan.host'],
    lifecycle_state: 'ready',
    pinned: false,
    idle_ttl_ms: 30 * MIN,
    egress: 'restricted',
    memory_max: 128 * 1024 * 1024,
    env_keys: ['WEBHOOK_SECRET'],
    active: {
      deployment_id: 'dpl_0c77b21',
      artifact_hash: '0c77b211a2c4e6f8a0b2c4d6e8f0a2b4c6e8f0a2',
      engine_version: 'bun 1.1.34',
    },
  },
];

export const previewDeployments = [
  {
    id: 'dpl_c81f2ae',
    app: 'pulse-widget',
    source_hash: 'c81f2ae1b3d5f7a9c1b3d5f7a9c1b3d5',
    engine_version: 'bun 1.2.19',
    artifact_hash: null,
    status: 'building',
    error: null,
    created_ms: ts(0),
    source: { kind: 'github', branch: 'main', commit: 'c81f2ae' },
  },
  {
    id: 'dpl_9f2c1a4',
    app: 'atelier',
    source_hash: '9f2c1a4b8e11d7a0c3f2e11a',
    engine_version: 'bun 1.2.19',
    artifact_hash: '9f2c1a4b8e11d7a0c3f2e11ad7a0c3f2e11ab9e02f1',
    status: 'active',
    error: null,
    created_ms: ts(2),
    source: { kind: 'github', branch: 'main', commit: '41c9e2f' },
  },
  {
    id: 'dpl_b7d0c11',
    app: 'ledger',
    source_hash: 'b7d0c11a3f9e2d8c1b4a6e0f',
    engine_version: 'bun 1.2.19',
    artifact_hash: 'b7d0c11a3f9e2d8c1b4a6e0f2d7c9b1a3e5f7d9c',
    status: 'active',
    error: null,
    created_ms: ts(5 * 60),
    source: { kind: 'github', branch: 'main', commit: '7e2d8a1' },
  },
  {
    id: 'dpl_77c9d3b',
    app: 'prism-docs',
    source_hash: '77c9d3b1a2c4e6f8d0b2a4c6',
    engine_version: 'bun 1.2.19',
    artifact_hash: '77c9d3b1a2c4e6f8d0b2a4c6e8f0a2b4c6e8f0a2',
    status: 'active',
    error: null,
    created_ms: ts(60 * 24),
    source: { kind: 'github', branch: 'main', commit: '3b1a2c4' },
  },
  {
    id: 'dpl_e33ab90',
    app: 'pr-412-checkout',
    source_hash: 'e33ab901c2d4f6a8e0b2c4d6',
    engine_version: 'bun 1.2.19',
    artifact_hash: 'e33ab901c2d4f6a8e0b2c4d6f8a0b2c4d6e8f0a2',
    status: 'sealed',
    error: null,
    created_ms: ts(60 * 3),
    source: { kind: 'github', branch: 'feat/checkout-v2', commit: 'e33ab90' },
  },
  {
    id: 'dpl_2ac401d',
    app: 'helios-api',
    source_hash: '2ac401d7e3b9f1a8c4d2e6f0',
    engine_version: 'bun 1.2.19',
    artifact_hash: '2ac401d7e3b9f1a8c4d2e6f0b1a3c5d7e9f1a2b3',
    status: 'active',
    error: null,
    created_ms: ts(60 * 48),
    source: { kind: 'github', branch: 'main', commit: '2ac401d' },
  },
  {
    id: 'dpl_a1e8f02',
    app: 'atelier',
    source_hash: 'a1e8f021c3d5f7a9b1c3d5f7',
    engine_version: 'bun 1.2.19',
    artifact_hash: 'a1e8f021c3d5f7a9b1c3d5f7a9b1c3d5f7a9b1c3',
    status: 'sealed',
    error: null,
    created_ms: ts(60 * 24),
    source: { kind: 'github', branch: 'main', commit: 'a1e8f02' },
  },
  {
    id: 'dpl_5f4e21c',
    app: 'atelier',
    source_hash: '5f4e21c1b3d5f7a9c1b3d5f7',
    engine_version: 'bun 1.2.19',
    artifact_hash: '5f4e21c1b3d5f7a9c1b3d5f7a9c1b3d5f7a9b1c3',
    status: 'sealed',
    error: null,
    created_ms: ts(60 * 48),
    source: { kind: 'github', branch: 'main', commit: '5f4e21c' },
  },
  {
    id: 'dpl_91d3e5f',
    app: 'atelier',
    source_hash: '91d3e5f1b3d5f7a9c1b3d5f7',
    engine_version: 'bun 1.2.19',
    artifact_hash: null,
    status: 'failed',
    error: 'build failed · module "edge-kv" resolves no entrypoint for target bun',
    created_ms: ts(60 * 24 * 3),
    source: { kind: 'github', branch: 'main', commit: '91d3e5f' },
  },
  {
    id: 'dpl_0c77b21',
    app: 'orbit-cron',
    source_hash: '0c77b211a2c4e6f8a0b2c4d6',
    engine_version: 'bun 1.1.34',
    artifact_hash: '0c77b211a2c4e6f8a0b2c4d6e8f0a2b4c6e8f0a2',
    status: 'active',
    error: null,
    created_ms: ts(60 * 24 * 6),
    source: { kind: 'github', branch: 'main', commit: '0c77b21' },
  },
  {
    id: 'dpl_e90b774',
    app: 'pr-409-i18n',
    source_hash: 'e90b7741a2c4e6f8a0b2c4d6',
    engine_version: 'bun 1.2.19',
    artifact_hash: 'e90b7741a2c4e6f8a0b2c4d6e8f0a2b4c6e8f0a2',
    status: 'sealed',
    error: null,
    created_ms: ts(60 * 48),
    source: { kind: 'github', branch: 'feat/i18n-routing', commit: 'e90b774' },
  },
];

// 60 oldest-first points for the series window.
const series = Array.from({ length: 60 }, (_, i) => {
  const wave = Math.sin(i / 5.2) * 6 + Math.sin(i / 2.3) * 3;
  const requests = Math.round(180 + wave * 12 + (i % 13 === 4 ? 60 : 0));
  const errors = i % 17 === 9 ? 3 : i % 29 === 12 ? 1 : 0;
  const p50 = Math.round((24 + wave) * 10) / 10;
  const p99 = Math.round((58 + wave * 2.4 + (i % 11 === 5 ? 26 : 0)) * 10) / 10;
  const cold = i % 23 === 7 ? 2 : 0;
  return { t: now - (60 - i) * 60_000, requests, errors, p50_ms: p50, p99_ms: p99, cold_starts: cold };
});

export const previewMetrics = {
  kind: 'router',
  window_seconds: 3600,
  totals: {
    requests_1m: 1284,
    rps_1m: 21.4,
    error_rate_1m: 0.03,
    p50_ms: 24,
    p99_ms: 58,
    requests_1h: 61200,
    error_rate_1h: 0.03,
    cold_starts_1h: 41,
    boot_p50_ms: 34,
    boot_p99_ms: 92,
  },
  series,
  boot_phases: {
    sample_count: 41,
    phases: [
      { name: 'namespaces_cgroup', p50_ms: 2.1 },
      { name: 'network', p50_ms: 0.4 },
      { name: 'mounts', p50_ms: 1.2 },
      { name: 'seccomp', p50_ms: 0.1 },
      { name: 'exec_runtime_init', p50_ms: 24.6 },
      { name: 'socket_ready', p50_ms: 3.9 },
    ],
  },
  apps: [
    { app: 'atelier', rps_1m: 214, requests_1h: 12840, error_rate_1m: 0.02, p50_ms: 22, p99_ms: 54 },
    { app: 'helios-api', rps_1m: 861, requests_1h: 51660, error_rate_1m: 0.04, p50_ms: 28, p99_ms: 71 },
    { app: 'ledger', rps_1m: 32, requests_1h: 1920, error_rate_1m: 0.0, p50_ms: 19, p99_ms: 41 },
    { app: 'prism-docs', rps_1m: 0, requests_1h: 0, error_rate_1m: 0.0, p50_ms: 0, p99_ms: 0 },
    { app: 'pulse-widget', rps_1m: 118, requests_1h: 7080, error_rate_1m: 0.0, p50_ms: 16, p99_ms: 38 },
    { app: 'orbit-cron', rps_1m: 2, requests_1h: 120, error_rate_1m: 0.0, p50_ms: 14, p99_ms: 22 },
  ],
};

export const previewEvents = [
  { time_ms: ts(2), type: 'deploy', app: 'atelier', message: 'dpl_9f2c1a4 promoted · blue-green swap in 0.3s' },
  { time_ms: ts(9), type: 'revival', app: 'pr-412-checkout', message: 'revived from cold in 41 ms' },
  { time_ms: ts(24), type: 'scale_to_zero', app: 'prism-docs', message: 'idle 10 min · cage reaped, artifact retained' },
  { time_ms: ts(64), type: 'crash', app: 'ledger', message: 'exceeded 256 MB — cage restarted (2nd in 24h)' },
  { time_ms: ts(60 * 3), type: 'crash_loop', app: 'orbit-cron', message: 'io_uring_setup blocked · seccomp filter held' },
  { time_ms: ts(60 * 24), type: 'cert_renewed', app: null, message: '*.swan.host wildcard renewed via DNS-01 · 90d' },
  { time_ms: ts(60 * 48), type: 'deploy', app: 'helios-api', message: 'dpl_2ac401d promoted · previous retained' },
  { time_ms: ts(60 * 72), type: 'domain_mapped', app: 'atelier', message: 'atelier.studio mapped · cert issuing' },
];

const REQ_PATHS = [
  ['GET', '/api/products?cursor=61', 'atelier', 200],
  ['GET', '/', 'atelier', 200],
  ['POST', '/v1/events', 'helios-api', 202],
  ['GET', '/v1/events?cursor=9a2', 'helios-api', 200],
  ['GET', '/api/balance/acct_912', 'ledger', 200],
  ['POST', '/api/checkout/intent', 'atelier', 201],
  ['GET', '/docs/cages/lifecycle', 'prism-docs', 200],
  ['GET', '/v1/beacon', 'pulse-widget', 200],
  ['POST', '/v1/beacon', 'pulse-widget', 204],
  ['GET', '/healthz', 'helios-api', 200],
  ['GET', '/api/products/psi-lamp', 'atelier', 200],
  ['POST', '/api/transfers', 'ledger', 201],
  ['GET', '/assets/type-spec.css', 'prism-docs', 200],
  ['GET', '/api/search?q=oak', 'atelier', 200],
  ['DELETE', '/v1/events/e_88', 'helios-api', 204],
];

// Deterministic 20-minute traffic pattern so the per-app minute sparks have
// real shape: helios-api carries most of the load, atelier breathes with a
// bursty retail rhythm, ledger and the rest tick along quietly.
export const previewRequests = (() => {
  const rows = [];
  let seq = 0x1a2b;
  for (let i = 0; i < 340; i++) {
    const p = REQ_PATHS[(i * 7) % REQ_PATHS.length];
    // Spread over ~20 minutes with a per-app wave so buckets differ.
    const minute = (i * 13) % 20;
    const wave = p[2] === 'helios-api' ? 1 : p[2] === 'atelier' ? Math.sin(minute / 3) ** 2 : 0.4;
    if (((i * 31) % 100) / 100 > 0.25 + wave * 0.6) continue;
    const withinMinute = (i * 2711) % 60_000;
    const cold = i % 47 === 11;
    const dur = cold ? 38 + (i % 5) * 6 : 3 + (i % 24);
    rows.push({
      time_ms: now - minute * 60_000 - withinMinute,
      request_id: `req_${(seq++).toString(16)}`,
      method: p[0],
      host: 'swan.host',
      app: p[2],
      path: p[1],
      status: i % 89 === 19 ? 500 : p[3],
      duration_ms: dur,
      cold,
      protocol: 'HTTP/2',
      bytes_in: p[0] === 'GET' ? 0 : 420,
      bytes_out: 1840 + (i % 9) * 220,
    });
  }
  return rows.sort((a, b) => b.time_ms - a.time_ms);
})();

export const previewGithub = {
  configured: true,
  app: { name: 'Cygnus Tenant Zero', owner: 'chasercat', html_url: 'https://github.com/apps/cygnus-tenant-zero' },
  repositories: [
    {
      installation_id: 12345678,
      repository_id: 9012,
      owner: 'chasercat',
      name: 'atelier',
      full_name: 'chasercat/atelier',
      default_branch: 'main',
      branch: 'main',
      app: 'atelier',
      domain: 'atelier.swan.host',
      engine_version: 'bun 1.2.19',
      entry: 'index.ts',
      private: false,
    },
    {
      installation_id: 12345678,
      repository_id: 9013,
      owner: 'chasercat',
      name: 'helios-api',
      full_name: 'chasercat/helios-api',
      default_branch: 'main',
      branch: 'main',
      app: 'helios-api',
      domain: 'helios-api.swan.host',
      engine_version: 'bun 1.2.19',
      entry: 'src/index.ts',
      private: true,
    },
  ],
  jobs: [
    {
      id: 'job_8a4f',
      owner: 'chasercat',
      name: 'atelier',
      full_name: 'chasercat/atelier',
      kind: 'push',
      pull_request: null,
      environment: 'production',
      sha: '41c9e2f1b3d5f7a9c1b3d5f7a9c1b3d5',
      branch: 'main',
      status: 'completed',
      attempts: 1,
      error: null,
    },
    {
      id: 'job_7c2e',
      owner: 'chasercat',
      name: 'pulse-widget',
      full_name: 'chasercat/pulse-widget',
      kind: 'push',
      pull_request: null,
      environment: 'production',
      sha: 'c81f2ae1b3d5f7a9c1b3d5f7a9c1b3d5',
      branch: 'main',
      status: 'running',
      attempts: 1,
      error: null,
      deployment_id: 'dpl_c81f2ae',
    },
    {
      id: 'job_5b91',
      owner: 'chasercat',
      name: 'atelier',
      full_name: 'chasercat/atelier',
      kind: 'push',
      pull_request: null,
      environment: 'production',
      sha: '91d3e5f1b3d5f7a9c1b3d5f7a9c1b3d5',
      branch: 'main',
      status: 'failed',
      attempts: 2,
      error: 'module "edge-kv" resolves no entrypoint for target bun',
    },
    {
      id: 'job_4d77',
      owner: 'chasercat',
      name: 'pr-412-checkout',
      full_name: 'chasercat/pr-412-checkout',
      kind: 'pull_request',
      pull_request: 412,
      environment: 'preview',
      sha: 'e33ab901c2d4f6a8e0b2c4d6f8a0b2c4',
      branch: 'feat/checkout-v2',
      status: 'completed',
      attempts: 1,
      error: null,
    },
    {
      id: 'job_3f0a',
      owner: 'chasercat',
      name: 'pr-409-i18n',
      full_name: 'chasercat/pr-409-i18n',
      kind: 'pull_request',
      pull_request: 409,
      environment: 'preview',
      sha: 'e90b7741a2c4e6f8a0b2c4d6e8f0a2b4',
      branch: 'feat/i18n-routing',
      status: 'completed',
      attempts: 1,
      error: null,
    },
  ],
};

// Egress fixture — NodeScreen renders it only in preview (no live source).
export const previewEgress = {
  today: '18.4 GB',
  conns: '124k',
  modes: { public: 31, restricted: 5, none: 249, open: 2 },
  top: [
    { app: 'helios-api', gb: 9.1 },
    { app: 'atelier', gb: 4.2 },
    { app: 'ledger', gb: 2.8 },
    { app: 'prism-docs', gb: 1.1 },
  ],
};

// Preview build log for the sealed atelier deploy — the porcelain terminal voice.
export const previewBuildLog = [
  { kind: 'head', text: 'cygnus deploy · atelier · main @ 41c9e2f' },
  { kind: 'dim', text: 'uploading source · 812 KB · 214 files' },
  { kind: 'ok', text: 'source received · sha256 9f2c…e11a' },
  { kind: 'head', text: 'build cage · ephemeral overlay · egress allowlist: github.com, registry.npmjs.org' },
  { kind: 'dim', text: 'lifecycle scripts: disabled (trustedDependencies)' },
  { kind: 'text', text: 'bun install · 214 packages · lockfile verified' },
  { kind: 'ok', text: 'install complete · 3.78s' },
  { kind: 'text', text: 'bun build --bytecode · target bun 1.2.19' },
  { kind: 'text', text: 'bundle.js   1.24 MB · 61 modules' },
  { kind: 'text', text: 'bundle.jsc  3.41 MB · bytecode cache' },
  { kind: 'ok', text: 'artifact sealed · content-addressed · ab9e02f1' },
  { kind: 'head', text: 'blue-green swap' },
  { kind: 'text', text: 'spawning cage · userns mntns pidns netns · seccomp v14' },
  { kind: 'text', text: 'shim bound /cygnus/io/app.sock · readiness ok' },
  { kind: 'text', text: 'route swapped atomically · draining previous cage' },
  { kind: 'ok', text: 'live · https://atelier.swan.host · cold-start budget 38 ms' },
];
