// M6: this route must be prerendered to a REAL static file (`mobile/index.html`) rather than
// only served via adapter-static's SPA fallback shell — `src-tauri-mobile/tauri.conf.json`
// points its window's initial `url` directly at that file (`frontendDist` still covers the
// WHOLE `build-mobile/` output, so absolute asset paths like `/_app/...` still resolve; only
// the entry HTML differs from the desktop build's `index.html`). The page has no server-only
// data dependency — every Tauri IPC call happens client-side inside `onMount`, which never
// runs during this prerender pass — so marking it prerenderable is safe.
export const prerender = true;
