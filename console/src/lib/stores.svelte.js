// Global UI state — Svelte 5 runes, module-level.

export const ui = $state({
  screen: 'overview', // overview | app | deploy | deploys | observe | node | settings
  appId: null,
  deployId: null,
  paletteOpen: false,
  shipOpen: false,
});

export function go(screen, opts = {}) {
  ui.screen = screen;
  ui.appId = opts.appId ?? ui.appId;
  ui.deployId = opts.deployId ?? null;
  ui.paletteOpen = false;
  ui.shipOpen = false;
  window.scrollTo({ top: 0 });
}

export function openApp(appId) {
  go('app', { appId });
}

export function openDeploy(appId, deployId) {
  go('deploy', { appId, deployId });
}
