// Deterministic duotone squircle palette for app identicons.
// Quiet duotones — the same set the porcelain instrument panel uses.

const DUO = [
  ['#dfe5ff', '#2c46f0'],
  ['#d9f3e6', '#0da55f'],
  ['#efe6ff', '#7857f0'],
  ['#ffe9d6', '#e07c1f'],
  ['#dbeefc', '#1273b8'],
  ['#fde3e8', '#d5476b'],
  ['#e4f0d5', '#5c8f1d'],
  ['#e8e9ee', '#3d4351'],
];

export function identicon(name) {
  const key = typeof name === 'string' && name.length ? name : 'app';
  let h = 0;
  for (let i = 0; i < key.length; i++) h = (h * 31 + key.charCodeAt(i)) >>> 0;
  const [bg, fg] = DUO[h % DUO.length];
  const angle = 30 + (h % 5) * 60;
  return { bg, fg, angle };
}
