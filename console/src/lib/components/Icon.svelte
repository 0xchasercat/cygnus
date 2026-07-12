<script>
  let { name, size = 18, stroke = 1.7 } = $props();

  const GLYPHS = {
    overview:
      '<circle cx="7" cy="7" r="2" fill="currentColor" stroke="none"/><circle cx="17" cy="7" r="2" fill="currentColor" stroke="none"/><circle cx="7" cy="17" r="2" fill="currentColor" stroke="none"/><circle cx="17" cy="17" r="2" fill="currentColor" stroke="none"/>',
    deploys: '<path d="M5 4.5h14"/><path d="M12 20V9.5"/><path d="M7.8 13.2 12 9l4.2 4.2"/>',
    observe: '<path d="M3 12.5h3.6L9 6.8l4.4 11.4 2.4-5.7H21"/>',
    node: '<rect x="4" y="4" width="16" height="16" rx="3.6"/><circle cx="9.2" cy="12" r="1.25" fill="currentColor" stroke="none"/><circle cx="14.8" cy="12" r="1.25" fill="currentColor" stroke="none"/>',
    settings:
      '<path d="M4 8h7.4"/><circle cx="14.4" cy="8" r="2.3"/><path d="M17.6 8H20"/><path d="M4 16h2.4"/><circle cx="9.6" cy="16" r="2.3"/><path d="M12.8 16H20"/>',
    search: '<circle cx="11" cy="11" r="6.4"/><path d="m19.8 19.8-3.3-3.3"/>',
    ship: '<path d="M12 19V5.5"/><path d="M5.8 11.2 12 5l6.2 6.2"/>',
    copy: '<rect x="9" y="9" width="11" height="11" rx="2.6"/><path d="M5 15V6.6A1.6 1.6 0 0 1 6.6 5H15"/>',
    check: '<path d="m5 12.6 4.4 4.4L19 7.2"/>',
    x: '<path d="m6 6 12 12M18 6 6 18"/>',
    chevR: '<path d="m9.5 5.5 6.5 6.5-6.5 6.5"/>',
    chevD: '<path d="m5.5 9.5 6.5 6.5 6.5-6.5"/>',
    back: '<path d="M11 5.5 4.5 12l6.5 6.5"/><path d="M5 12h15"/>',
    ext: '<path d="M7.5 16.5 16.5 7.5"/><path d="M9.5 7.5h7v7"/>',
    branch:
      '<circle cx="6.5" cy="6" r="2.1"/><circle cx="6.5" cy="18" r="2.1"/><circle cx="17.5" cy="8.5" r="2.1"/><path d="M6.5 8.1v7.8"/><path d="M17.5 10.6c0 5.4-11 2.6-11 5.3"/>',
    clock: '<circle cx="12" cy="12" r="8.4"/><path d="M12 7.6V12l3 2.1"/>',
    dots: '<circle cx="5.5" cy="12" r="1.6" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.6" fill="currentColor" stroke="none"/><circle cx="18.5" cy="12" r="1.6" fill="currentColor" stroke="none"/>',
    terminal: '<path d="m5 7.5 4.8 4.5L5 16.5"/><path d="M12.5 17H19"/>',
    lock: '<rect x="5" y="10.5" width="14" height="9.5" rx="2.6"/><path d="M8.2 10.5V8a3.8 3.8 0 0 1 7.6 0v2.5"/>',
    key: '<circle cx="8" cy="14.5" r="3.6"/><path d="m10.8 11.7 8-8"/><path d="m15.3 7.2 2.6 2.6"/>',
    globe:
      '<circle cx="12" cy="12" r="8.4"/><path d="M3.8 12h16.4"/><path d="M12 3.6c2.9 3 2.9 13.8 0 16.8M12 3.6c-2.9 3-2.9 13.8 0 16.8"/>',
    zap: '<path d="M13 3.5 5.5 13h5L11 20.5 18.5 11h-5L13 3.5Z"/>',
    rollback: '<path d="M8.4 4.8 4.2 9l4.2 4.2"/><path d="M4.6 9h9a5.7 5.7 0 1 1-.3 11.4H9"/>',
    filter: '<path d="M4 6.5h16M7.2 12h9.6M10.4 17.5h3.2"/>',
    plus: '<path d="M12 5v14M5 12h14"/>',
    user: '<circle cx="12" cy="8.2" r="3.5"/><path d="M5.2 19.6c1.4-3.2 3.9-4.8 6.8-4.8s5.4 1.6 6.8 4.8"/>',
    folder:
      '<path d="M3.5 8.2a2.2 2.2 0 0 1 2.2-2.2h3.6l2.1 2.4h7a2.2 2.2 0 0 1 2.2 2.2v6.9a2.2 2.2 0 0 1-2.2 2.2H5.7a2.2 2.2 0 0 1-2.2-2.2V8.2Z"/>',
    eye: '<path d="M3.2 12S6.6 5.8 12 5.8 20.8 12 20.8 12 17.4 18.2 12 18.2 3.2 12 3.2 12Z"/><circle cx="12" cy="12" r="2.5"/>',
    arrowR: '<path d="M4.5 12h15"/><path d="m13.5 6 6 6-6 6"/>',
  };
</script>

<svg
  width={size}
  height={size}
  viewBox="0 0 24 24"
  fill="none"
  stroke="currentColor"
  stroke-width={stroke}
  stroke-linecap="round"
  stroke-linejoin="round"
  aria-hidden="true"
>
  {@html GLYPHS[name] ?? ''}
</svg>
