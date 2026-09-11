# Repository Guidelines

## Project Structure & Module Organization

Directory UI Viewer is a local-first desktop application with a web UI. Keep React and TypeScript code in `src/`, static files in `public/`, end-to-end tests in `e2e/`, and the Tauri/Rust shell in `src-tauri/`. Unit tests should sit beside the module they cover as `*.test.ts` or `*.test.tsx`. Personal project notes live in the Git-ignored `.project-notes/` Obsidian Vault.

## Build, Test, and Development Commands

- `npm install` installs the locked dependencies.
- `npm run dev` starts the browser-only Vite UI.
- `npm run tauri dev` starts the desktop application.
- `npm run build` type-checks and builds the frontend.
- `npm test` runs Vitest once; `npm run test:watch` runs it interactively.
- `npm run test:e2e` runs Playwright tests.
- `npm run lint` and `npm run format:check` validate code style.
- `cargo check --manifest-path src-tauri/Cargo.toml` checks Rust code.

Use Node.js 24 LTS as declared in `.nvmrc` and the exact npm version from `packageManager`.

## Coding Style & Naming Conventions

Prettier is authoritative for formatting. Use two-space indentation, TypeScript strict mode, `PascalCase` for React components, `camelCase` for functions and variables, and `kebab-case` for feature directories. Keep filesystem access behind a typed adapter; UI components must not depend directly on Tauri APIs. Rust follows `rustfmt`, with commands and modules named in `snake_case`.

## Testing Guidelines

Use Vitest and React Testing Library for components and logic, and Playwright for user journeys. Cover empty directories, permission errors, symlink behavior, large trees, and cancellation. Add a regression test with bug fixes when practical. Tests must not inspect or mutate real user files; use temporary fixtures.

## Security & Filesystem Access

Filesystem access is read-only and user-initiated. Never grant a static whole-disk scope. Canonicalize selected paths, restrict traversal to descendants of the selected root, preserve OS permission failures, and avoid following symlinks outside the approved tree. Do not log filenames or absolute paths without explicit diagnostic consent.

## Commit & Pull Request Guidelines

Use focused Conventional Commits, for example `feat: add directory picker` or `fix: contain symlink traversal`. Pull requests must explain purpose, security implications, validation performed, and linked issues. Include screenshots for UI changes. Never commit `.project-notes/`, credentials, real directory listings, build output, or editor state.
