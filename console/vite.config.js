import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { viteSingleFile } from 'vite-plugin-singlefile';

// singlefile inlines JS+CSS into one dist/index.html — used to publish the
// clickable concept as a single artifact. Remove it when this becomes a real app.
export default defineConfig({
  plugins: [svelte(), viteSingleFile()],
});
