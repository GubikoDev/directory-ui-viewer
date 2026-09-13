# Repository Guidelines

## Project Structure & Module Organization

Directory UI Viewer is a local-first desktop application with a web UI. Keep React and TypeScript code in `src/`, static files in `public/`, end-to-end tests in `e2e/`, and the Tauri/Rust shell in `src-tauri/`. Unit tests should sit beside the module they cover as `*.test.ts` or `*.test.tsx`. Personal project notes live in the `project-notes/` Obsidian Vault, which has its own local Git repository and is ignored by the application repository.

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

## Project Memory & Agent Collaboration

Treat `project-notes/` as active project memory, not an archive. Before substantive work, read `Home.md` and the notes relevant to the task. Record durable decisions with rationale in `Decisions.md`, update completed work and next actions in `Session Log.md`, and keep `Roadmap.md` current. Maintain wiki links and `Project Map.canvas` when relationships change; omit routine command noise.

For direct Codex–Claude consultation, read `project-notes/Agent Dialogue.md` and follow its CLI, session ownership, and transcript rules.

Codex and Claude share these instructions through `CLAUDE.md` and coordinate through `Agent Handoff.md`. Before continuing another agent's work, inspect the handoff, current files, and Git state. Record ownership, changed paths, validation, unresolved questions, and the next concrete action. Verify another agent's output before relying on it, preserve concurrent changes, and leave consequential unresolved choices to the user. After each coherent Vault document update, immediately commit it in the independent Vault Git repository and push to its existing private origin; do not wait for a separate user request. Verify push success and report failures without force-pushing. Exclude credentials, raw dialogue logs, and editor state. Commit Vault documents only in their independent local Git repository. Do not add the Vault to the application repository or publish it without an explicit user request.

## Commit & Pull Request Guidelines

Use focused Conventional Commits, for example `feat: add directory picker` or `fix: contain symlink traversal`. Pull requests must explain purpose, security implications, validation performed, and linked issues. Include screenshots for UI changes. Never commit `project-notes/` to the application repository. Never commit credentials, real directory listings, build output, or editor state to either repository.
