import adapter from "@sveltejs/adapter-static";
import { vitePreprocess } from "@sveltejs/vite-plugin-svelte";

// Mobile Thin-Client plan phase 3 (M6): one SvelteKit project ships TWO Tauri shells — the
// desktop cockpit (root `+page.svelte`) and the mobile app (`src/routes/mobile/`). Both routes
// are always built together (same route tree, same bundle graph); what differs per target is
// only WHICH prerendered HTML file each Tauri app's `frontendDist`/window `url` points at, and
// where adapter-static writes its output. `BUILD_TARGET=mobile` (set by the `build:mobile` npm
// script) is the single switch for the latter — see `package.json` and
// `src-tauri-mobile/tauri.conf.json` (`app.windows[0].url: "mobile"`) for the two ends of this.
const outDir = process.env.BUILD_TARGET === "mobile" ? "build-mobile" : "build";

/** @type {import('@sveltejs/kit').Config} */
const config = {
  preprocess: vitePreprocess(),
  kit: {
    adapter: adapter({ fallback: "index.html", pages: outDir, assets: outDir }),
  },
};

export default config;
