# Claude Code Project Instructions

@AGENTS.md

## Communication

- Respond in Korean unless the user requests another language.
- Use a concise, professional, and polite tone; do not use informal Korean speech.
- State assumptions and blockers explicitly instead of silently expanding scope.

## Working Method

- Inspect the relevant files and `git status` before changing code.
- Make a short, ordered plan, then implement and verify each step.
- Preserve unrelated user changes and keep commits focused with Conventional Commit messages.
- Do not implement product features when the request is limited to setup, review, or diagnosis.

## Project Context

- Read `README.md` for the product overview and `package.json` for authoritative scripts.
- Use Node.js 24.21.0 LTS and npm 11.19.0 as pinned by the repository.
- The UI is React/TypeScript; native filesystem boundaries belong in Rust/Tauri.
- Keep UI code independent of direct Tauri calls by using typed adapters.

## Security Requirements

- Directory access must be user-initiated, read-only, and limited to the selected root for the current session.
- Never add a static whole-disk filesystem scope or bypass macOS/Linux permissions.
- Canonicalize paths and prevent path traversal or symlink escape before returning filesystem data.
- Never use real user directories as test fixtures or log absolute paths without explicit consent.

## Private Project Memory

- If `.project-notes/` exists, read `Home.md` and relevant linked notes before architecture or roadmap work.
- Record material decisions in `Decisions.md` and completed work in `Session Log.md`.
- `.project-notes/` is private local context. Never stage, commit, publish, or copy its contents into public artifacts.
- Put personal Claude overrides in `CLAUDE.local.md`; it must remain untracked.

## Verification

- Run the smallest relevant checks while iterating.
- Before committing code, run `npm run format:check`, `npm run lint`, `npm run typecheck`, and relevant tests.
- For Rust changes, also run `cargo fmt --manifest-path src-tauri/Cargo.toml --check` and `cargo check --manifest-path src-tauri/Cargo.toml --locked` when system dependencies are available.
