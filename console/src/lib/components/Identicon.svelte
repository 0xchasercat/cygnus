<script>
  import { identicon } from '../identicon.js';

  let { name, size = 36 } = $props();
  const c = $derived(identicon(name));
  // hard-stop diagonal fold — origami, like the swan
  const style = $derived(
    `width:${size}px;height:${size}px;` +
      `background:linear-gradient(${c.angle}deg, ${c.fg} 0%, ${c.fg} 37%, ${c.bg} 37.5%);`
  );
</script>

<span class="identicon" {style}></span>
