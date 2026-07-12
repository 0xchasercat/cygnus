<script>
  // Latency chart — p50 in cobalt with soft fill, p99 as a quiet dashed trace.
  let { series = [], h = 230 } = $props();

  const W = 1000;
  const PAD = { t: 14, r: 8, b: 22, l: 40 };

  const maxY = $derived.by(() => {
    const m = Math.max(...series.map((d) => d[1]), 1);
    return Math.ceil(m / 40) * 40;
  });

  function pathFor(idx) {
    const iw = W - PAD.l - PAD.r;
    const ih = h - PAD.t - PAD.b;
    return series
      .map((d, i) => {
        const x = PAD.l + (i / (series.length - 1)) * iw;
        const y = PAD.t + ih - (d[idx] / maxY) * ih;
        return `${i === 0 ? 'M' : 'L'}${x.toFixed(1)} ${y.toFixed(1)}`;
      })
      .join(' ');
  }

  const p50 = $derived(pathFor(0));
  const p99 = $derived(pathFor(1));
  const area = $derived(
    `${p50} L${W - PAD.r} ${h - PAD.b} L${PAD.l} ${h - PAD.b} Z`
  );
  const gridYs = $derived(
    [0.25, 0.5, 0.75, 1].map((f) => ({
      y: PAD.t + (h - PAD.t - PAD.b) * (1 - f),
      v: Math.round(maxY * f),
    }))
  );
</script>

<svg viewBox="0 0 {W} {h}" style="width:100%;height:auto;display:block" aria-hidden="true">
  <defs>
    <linearGradient id="latfill" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="var(--cobalt)" stop-opacity="0.1" />
      <stop offset="1" stop-color="var(--cobalt)" stop-opacity="0" />
    </linearGradient>
  </defs>

  {#each gridYs as g}
    <line x1={PAD.l} y1={g.y} x2={W - PAD.r} y2={g.y} stroke="var(--line-2)" stroke-width="1" />
    <text
      x={PAD.l - 10}
      y={g.y + 3.5}
      text-anchor="end"
      font-family="var(--mono)"
      font-size="10.5"
      fill="var(--ink-3)">{g.v}</text
    >
  {/each}
  <line
    x1={PAD.l}
    y1={h - PAD.b}
    x2={W - PAD.r}
    y2={h - PAD.b}
    stroke="var(--line)"
    stroke-width="1"
  />

  <path d={area} fill="url(#latfill)" />
  <path
    d={p99}
    fill="none"
    stroke="var(--ink-4)"
    stroke-width="1.4"
    stroke-dasharray="3 5"
    stroke-linecap="round"
  />
  <path
    d={p50}
    fill="none"
    stroke="var(--cobalt)"
    stroke-width="1.8"
    stroke-linejoin="round"
    stroke-linecap="round"
  />

  <text
    x={PAD.l}
    y={h - 6}
    font-family="var(--mono)"
    font-size="10.5"
    fill="var(--ink-3)">-60 min</text
  >
  <text
    x={W - PAD.r}
    y={h - 6}
    text-anchor="end"
    font-family="var(--mono)"
    font-size="10.5"
    fill="var(--ink-3)">now</text
  >
</svg>
