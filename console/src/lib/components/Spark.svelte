<script>
  let { data = [], w = 84, h = 26, color = 'var(--cobalt)' } = $props();

  const pts = $derived.by(() => {
    if (!data.length) return '';
    const min = Math.min(...data);
    const max = Math.max(...data);
    const span = max - min || 1;
    return data
      .map((v, i) => {
        const x = (i / (data.length - 1)) * (w - 2) + 1;
        const y = h - 2 - ((v - min) / span) * (h - 5);
        return `${x.toFixed(1)},${y.toFixed(1)}`;
      })
      .join(' ');
  });
</script>

<svg width={w} height={h} viewBox="0 0 {w} {h}" aria-hidden="true" style="display:block">
  <polyline
    points={pts}
    fill="none"
    stroke={color}
    stroke-width="1.5"
    stroke-linecap="round"
    stroke-linejoin="round"
  />
  <polygon
    points="1,{h - 1} {pts} {w - 1},{h - 1}"
    fill={color}
    opacity="0.07"
    stroke="none"
  />
</svg>
