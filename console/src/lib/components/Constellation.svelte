<script>
  // The Cygnus asterism (Northern Cross) — brand flourish, drawn as
  // drafting-mark stars. Decorative only.
  let { w = 240, labeled = true, opacity = 1 } = $props();

  const stars = [
    { x: 30, y: 18, m: 3, id: 'kappa' },
    { x: 46, y: 28, m: 3, id: 'iota' },
    { x: 68, y: 42, m: 4, id: 'delta' },
    { x: 128, y: 20, m: 6, id: 'deneb', label: 'Deneb' },
    { x: 106, y: 54, m: 5, id: 'sadr', label: 'Sadr' },
    { x: 142, y: 76, m: 4, id: 'gienah' },
    { x: 168, y: 94, m: 3, id: 'zeta' },
    { x: 92, y: 76, m: 3, id: 'eta' },
    { x: 72, y: 104, m: 4, id: 'albireo', label: 'Albireo' },
  ];
  const links = [
    ['kappa', 'iota'],
    ['iota', 'delta'],
    ['delta', 'sadr'],
    ['sadr', 'gienah'],
    ['gienah', 'zeta'],
    ['deneb', 'sadr'],
    ['sadr', 'eta'],
    ['eta', 'albireo'],
  ];
  const at = (id) => stars.find((s) => s.id === id);
</script>

<svg
  width={w}
  height={(w * 120) / 200}
  viewBox="0 0 200 120"
  aria-hidden="true"
  style="display:block;opacity:{opacity}"
>
  {#each links as [a, b]}
    <line
      x1={at(a).x}
      y1={at(a).y}
      x2={at(b).x}
      y2={at(b).y}
      stroke="var(--ink)"
      stroke-opacity="0.13"
      stroke-width="0.7"
    />
  {/each}
  {#each stars as s}
    <path
      d="M{s.x} {s.y - s.m} v{s.m * 2} M{s.x - s.m} {s.y} h{s.m * 2}"
      stroke={s.m >= 5 ? 'var(--cobalt)' : 'var(--ink)'}
      stroke-opacity={s.m >= 5 ? 0.85 : 0.4}
      stroke-width="1.1"
      stroke-linecap="round"
    />
    {#if labeled && s.label}
      <text
        x={s.x + s.m + 4}
        y={s.y + 3}
        font-family="var(--mono)"
        font-size="7.5"
        letter-spacing="0.08em"
        fill="var(--ink-3)">{s.label}</text
      >
    {/if}
  {/each}
</svg>
