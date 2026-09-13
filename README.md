# Directory UI Viewer

A local-first visual directory explorer for macOS and Linux. The project aims to make filesystem structures understandable to non-developers while remaining efficient for developers.

## Status

The repository currently contains the project foundation only. Directory browsing is intentionally not implemented yet.

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

The application will ask the user to choose a root directory through the native picker. A future Rust boundary will canonicalize that path and expose only read-only directory metadata within the selected tree. Access will not bypass operating-system permissions, follow escaping symlinks, persist silently, or grant a static whole-disk scope.

## Project Notes

Open `project-notes/` as an Obsidian Vault. It is an independent local Git
repository, excluded from this application repository. Commit documentation
changes from inside that folder and push to its separately configured private
remote. Obsidian settings are untracked. The Vault is not included when cloning
the application.

## License

[MIT](LICENSE)
