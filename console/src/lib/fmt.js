// One shared number util. Bytes → MB/GB 1dp; ms → "12 ms"/"1.2 s";
// rates 1dp; percentages. Mono-instrument voice.

export function bytes(n) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '—';
  if (v < 1024) return `${v} B`;
  const mb = v / (1024 * 1024);
  if (mb < 1) return `${(v / 1024).toFixed(1)} KB`;
  if (mb < 1024) return `${mb.toFixed(1)} MB`;
  return `${(mb / 1024).toFixed(1)} GB`;
}

export function rate(n) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '0';
  if (v > 0 && v < 10) return v.toFixed(1);
  return Math.round(v).toLocaleString();
}

export function millis(n) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '— ms';
  if (v < 1000) return `${Math.round(v)} ms`;
  return `${(v / 1000).toFixed(1)} s`;
}

export function percent(n, digits = 2) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '—';
  return `${v.toFixed(digits)}%`;
}

export function shortHash(value) {
  if (!value) return '—';
  if (value.length <= 12) return value;
  return `${value.slice(0, 7)}…${value.slice(-4)}`;
}

export function int(n) {
  const v = Number(n);
  if (!Number.isFinite(v)) return '—';
  return Math.round(v).toLocaleString();
}

// Cold-boot phase names arrive as the daemon's snake_case identifiers.
const PHASE_LABELS = {
  namespaces_cgroup: 'namespaces + cgroup',
  network: 'network',
  mounts: 'mounts',
  seccomp: 'seccomp',
  exec_runtime_init: 'exec + runtime init',
  socket_ready: 'socket ready',
};

export function phaseLabel(name) {
  return PHASE_LABELS[name] ?? String(name).replaceAll('_', ' ');
}

// ANSI escape sequences (CSI colors/cursor moves, OSC titles, single-char
// escapes). Build tools emit them even when piped; rendered raw they turn
// logs into per-character garbage. Stripped at display time — stored logs
// stay byte-exact for the CLI, where a real terminal interprets them.
// eslint-disable-next-line no-control-regex
const ANSI_PATTERN = /\x1b(?:\[[0-9;?]*[ -/]*[@-~]|\][^\x07\x1b]*(?:\x07|\x1b\\)?|[0-~])/g;

export function stripAnsi(text) {
  return String(text).replace(ANSI_PATTERN, '');
}
