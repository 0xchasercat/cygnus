// One shared relative-time util. Same voice everywhere: "just now",
// "Nm ago", "Nh ago", then a short month/day stamp for older entries.

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];

export function relativeTime(ms, now = Date.now()) {
  if (ms == null || Number.isNaN(Number(ms))) return '';
  const t = Number(ms);
  const diff = Math.max(0, now - t);
  if (diff < 45_000) return 'just now';
  if (diff < 60_000) return '1m ago';
  const mins = Math.floor(diff / 60_000);
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  const d = new Date(t);
  return `${MONTHS[d.getMonth()]} ${d.getDate()}`;
}

// Humanized uptime from seconds: "41d 7h", "2h 13m", "9m 4s".
export function uptime(seconds) {
  const s = Math.max(0, Math.floor(Number(seconds) || 0));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ${m % 60}m`;
  const d = Math.floor(h / 24);
  return `${d}d ${h % 24}h`;
}
