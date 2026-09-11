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

## Filesystem Security Direction

The application will ask the user to choose a root directory through the native picker. A future Rust boundary will canonicalize that path and expose only read-only directory metadata within the selected tree. Access will not bypass operating-system permissions, follow escaping symlinks, persist silently, or grant a static whole-disk scope.

## License

[MIT](LICENSE)
