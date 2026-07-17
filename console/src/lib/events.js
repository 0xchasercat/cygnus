// Event icon/tone maps — factored out of Overview + Observe so the live
// event feed reads identically in both places. Tones are the porcelain
// palette: cobalt for deploys/revival, ghost for neutral, red for crashes.

export const EVENT_ICON = {
  deploy: 'ship',
  deploy_failed: 'ship',
  rollback: 'rollback',
  revival: 'zap',
  scale_to_zero: 'clock',
  crash: 'node',
  crash_loop: 'node',
  engine_registered: 'node',
  cert_issued: 'globe',
  cert_renewed: 'globe',
  cert_failed: 'globe',
  domain_mapped: 'globe',
};

export const TONE_FG = {
  live: '#087a45',
  cobalt: 'var(--cobalt-deep)',
  ghost: 'var(--ink-3)',
  amber: '#a36a06',
  red: '#b02c23',
};

export const TONE_BG = {
  live: 'var(--live-soft)',
  cobalt: 'var(--cobalt-ghost)',
  ghost: 'var(--surface-3)',
  amber: 'var(--amber-soft)',
  red: 'var(--red-soft)',
};

export function eventTone(type) {
  switch (type) {
    case 'deploy':
    case 'rollback':
    case 'revival':
    case 'engine_registered':
      return 'cobalt';
    case 'deploy_failed':
    case 'crash':
    case 'crash_loop':
    case 'cert_failed':
      return 'red';
    case 'scale_to_zero':
      return 'amber';
    default:
      return 'ghost';
  }
}

export function eventIcon(type) {
  return EVENT_ICON[type] ?? 'node';
}

export function eventStyle(type) {
  const tone = eventTone(type);
  return `color:${TONE_FG[tone]};background:${TONE_BG[tone]}`;
}
