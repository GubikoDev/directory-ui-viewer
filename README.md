# Directory UI Viewer

A local-first visual directory explorer for macOS and Linux. The project aims to make filesystem structures understandable to non-developers while remaining efficient for developers.

## Status

The internal directory listing, usage aggregation, cancellation, and typed
adapters are implemented with a temporary verification screen. Native security,
whole-application resource accounting, and macOS integration acceptance remain
in progress; unit tests and engine benchmarks alone do not establish completion.

The first implementation targets macOS. Rust CI checks both macOS and Linux;
Linux release support remains a later milestone. The frontend uses a typed
filesystem adapter, with native folder selection and root approval owned by
Rust. The frontend has no direct dialog permission.

## Technology

- React, TypeScript, Vite, Tailwind CSS, and shadcn/ui conventions
- Tauri 2 desktop shell
- Vitest, React Testing Library, and Playwright
- ESLint, Prettier, rustfmt, and GitHub Actions

## Getting Started

Prerequisites are Node.js 24.21.0 LTS, npm 11.19.0, Rust, and the [Tauri system dependencies](https://v2.tauri.app/start/prerequisites/) for your OS.

```bash
npm install
npm run tauri dev
```

For frontend-only development, run `npm run dev`.

### macOS development

Use the Node version in `.nvmrc`; its bundled npm matches `packageManager`.
With [fnm](https://github.com/Schniz/fnm) installed:

```bash
eval "$(fnm env --shell zsh)"
fnm install
fnm use
npm ci
npx playwright install chromium
npm run tauri dev
```

After copying the repository from Linux, run `npm ci` to replace native
dependencies with macOS builds. If linked worktrees were copied too, repair
their paths with `git worktree repair <copied-worktree-path>`.

Validate the environment with `npm run build`, `npm test`, `npm run lint`,
`npm run format:check`, `npm run test:e2e`, and
`cargo check --manifest-path src-tauri/Cargo.toml --locked`.

## Filesystem Security Direction

The native picker approves the successfully opened directory object. Safely
opened descendant directory objects remain approved after moves; refresh keeps
the original root handle. A replacement at the old path is never automatically
approved. The read-only boundary preserves mount boundaries and OS permission
failures, rejects symlink target traversal, and revokes new operations on close
while existing calls drain. There is no static whole-disk scope.

## Internal app measurement

See [the measurement procedure](e2e/MEASUREMENT.md) for fixed temporary fixtures
and the opt-in measurement build. Its reports distinguish fixture results from
the actual macOS adapter and retain incomplete runs and failed comparisons.

## Project Notes

Open `project-notes/` as an Obsidian Vault. It is an independent local Git
repository, excluded from this application repository. Commit documentation
changes from inside that folder and push to its separately configured private
remote. Obsidian settings are untracked. The Vault is not included when cloning
the application.

## License

[MIT](LICENSE)
