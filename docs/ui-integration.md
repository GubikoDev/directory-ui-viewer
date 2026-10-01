# UI v2 — first engine integration (2026-10-01)

The Figma UI v2 screens now use the existing SessionClient/ExplorerStore and
FilesystemAdapter. Desktop selects NativeAdapter; browser uses FixtureAdapter.
No production filesystem permissions, native engine or IPC contracts changed.

Reference: [Directory UI Viewer — UI v2](https://www.figma.com/design/OCUkptt2r1GB3ysaGjqs11?node-id=5-77).
Current design context was read for organization, mindmap, empty, loading,
file list, detail, refresh confirmation and light screens. Theme variables were
read from Figma. SVG icons were downloaded into public/design; edges and node
placement are generated from engine observations rather than example images.

## Connected actions

- Open folder: chooseRoot, then initial directory listing and usage scan.
- Select/expand folder: select/setExpanded and lazy directory listing.
- Node file control: list(files), closeListing, nextPage.
- Detail: current generation metadata and logical/allocated usage observations.
- Stop: cancel each currently active task; usage: scan.
- Read again: confirmation then refresh; session close: close.
- Organization/mindmap share the same store. Hidden filtering and theme are local
  presentation state and do not restart scanning.

Unknown values display a dash, unavailable values remain unavailable, and partial
usage retains its plus marker. No logical-size fallback for allocated usage.
InternalApp preserves the prior verification surface behind
VITE_INTERNAL_BENCHMARK=1.

## Command verification

```sh
npm run build
npm run lint
npm run format:check
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --manifest-path src-tauri/Cargo.toml --example ui_smoke_bridge
DUV_NATIVE_UI_SMOKE=1 npm test
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo test --manifest-path src-tauri/Cargo.toml --test macos_native_runtime
npm run test:e2e -- --reporter=line
```

Results: TypeScript120 (including native UI smoke), macOS runtime18, browser3,
build/lint/format/diff checks passed. Without DUV_NATIVE_UI_SMOKE, the native UI
smoke is intentionally skipped and119 tests run.

The native smoke renders the real App in jsdom, uses SessionClient and
NativeAdapter, and replaces Tauri invoke delivery with a test-only stdio process
running the production NativeRuntime/walkdir engine. The process creates/removes
its own temporary fixture and accepts no frontend paths. Folder open, directory
and file listing, usage response, refresh generation2 and close were observed.
It verifies connection and response rendering, not Tauri proof delivery,
WebView lifecycle or native picker interactions. The actual picker/WebView
journey was not manually exercised in this pass.

A Chromium screenshot and visible icon loading/dimensions were checked with
minimal interaction. The browser fixture has no supplied usage observations,
so its node values correctly remain unknown.

## Phase-one limits / next pass

Completion is engine response to UI calls, per the user's current criterion.
Layout is an initial dynamic implementation, not pixel-perfect final acceptance.
Branches show12 folders initially, load/show more on demand, and the canvas
limits rendering to240 nodes. Large-tree layout/virtualization, panel collisions,
zoom/pan, detailed error copy, full accessibility and native picker/WebView
journeys are follow-up work. No performance or full correctness acceptance is
claimed. Figma examples do not become hardcoded filesystem observations.
