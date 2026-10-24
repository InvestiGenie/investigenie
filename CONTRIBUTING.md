# Contributing

Thanks for helping. Keep changes small, tested, and focused.

## Setup

Requirements: stable Rust, and Node.js 20.19+ or 22.12+ with npm. From `investigenie/`:

```sh
npm install
cargo run -p investigenie-api     # API on http://127.0.0.1:3000 (quickstart storage)
npm run dev:web                   # UI on http://127.0.0.1:5173
```

You don't need a database: quickstart stores encrypted data in `investigenie_data/` next to the binary. Datasource and AI credentials are entered in the UI. Never put them in `.env`, code, tests, or logs.

## Quality gates

Install the Git hooks once per clone ([pre-commit](https://pre-commit.com) 3.2+, the Rust/Node/Python toolchains, and ShellCheck and Hadolint). Trivy uses a local executable when available; otherwise it uses the pinned Docker image. Install either [Trivy](https://trivy.dev) or Docker.

```sh
pipx install pre-commit        # or: pip install pre-commit
pre-commit install             # installs both the pre-commit and pre-push hooks
```

| Stage | Checks |
| --- | --- |
| Every commit | Whitespace, final newlines, YAML/JSON/TOML syntax, merge markers, large files, private keys, secrets (gitleaks), `rustfmt`, Python lint and format (ruff), shell (shellcheck), Dockerfile (hadolint), workflows (actionlint, zizmor), playground unit tests |
| Every push | `cargo clippy -D warnings`, frontend typecheck, Trivy (fixable HIGH/CRITICAL vulnerabilities, secrets, misconfigurations) |

Run everything on demand with `pre-commit run --all-files --hook-stage manual`. Skip a hook once with `SKIP=<id> git commit ...`, never with `--no-verify` on shared branches. Accepted Trivy findings go in `.trivyignore` with a reason and a review date.

`backend/migrations/` is excluded from the whitespace fixers on purpose: sqlx checksums applied migrations, so even a trailing newline would break existing PostgreSQL deployments. Add a new migration instead of editing one.

CI (`.github/workflows/ci.yml`) also scans the checked-out repository with Gitleaks; the local Gitleaks hook scans staged changes. CI runs the same hooks plus the full Rust test suite (including the PostgreSQL tests against a service container), the frontend tests and build, the playground tests on Python 3.9 and 3.13, a Docker image build with a smoke test and a Trivy image scan, and dependency review on pull requests. CodeQL (`.github/workflows/codeql.yml`) runs on pull requests and weekly. The playground evaluation (`.github/workflows/playground-eval.yml`) runs the AI investigation flow nightly on `qwen2.5:1.5b`; trigger it manually with other models from the Actions tab.

## Before you open a PR

```sh
pre-commit run --all-files --hook-stage manual
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

- Add or update tests for every behaviour change. Backend upstreams are mocked with `wiremock`; frontend tests use Vitest + Testing Library.
- The codebase is `rustfmt`-clean and CI enforces it, so format everything you touch.
- Update the docs in `docs/` or `README.md` when behaviour, configuration, or APIs change.

## Conventions

- Shared connector contracts and registration belong in `backend/src/data_connectors.rs` and `backend/src/ai_connectors.rs`; keep concrete implementations under `integrations/data_connectors/<id>/` and `integrations/ai_connectors/<id>/`.
- Data connectors import `crate::data_connectors` and are auto-discovered by `backend/build.rs`. AI transports implement `crate::ai_connectors::AiConnector` and are explicitly registered in the backend AI contract; do not mix the two registries.

- **Security first:** datasources stay read-only. Never return or log secrets. Every limit (size, time, concurrency) stays bounded.
- **No datasource special-casing in shared code:** add the capability to the integration descriptor instead.
- **Errors:** `{"error":{"code","message"}}` with a stable `code`, and no upstream bodies or credentials.
- **Saved objects:** writes use revisions (`If-Match`); keep them that way.
- **Knowledge:** manual entries are encrypted in both storage modes and bounded to 128 entries with 32 KiB content each. Completed-result promotion is user-triggered and always creates an unverified draft through existing CRUD; never auto-publish findings. Keep Markdown previews free of raw HTML and external images. Publication is not verification; retrieval and investigation citation snapshots are a separate milestone. PostgreSQL upgrades require the new knowledge migration.
- **UI:** accessible labels and roles (the tests rely on them), and both themes styled.
- **Commits:** imperative subject ≤72 characters, plus a short body explaining what changed and why.

## Where to start

- [Architecture](docs/architecture.md): how the parts fit.
- [Backend](docs/backend.md) and [Frontend](docs/frontend.md): code maps.
- [Integrations](docs/integrations.md): add a datasource (good first contribution).

## Releases (maintainers)

1. Bump the Cargo workspace and frontend package versions and matching lock entries; update the workflow's manual tag default.
2. Obtain approval for a short commit message and user-facing release notes. Save the approved notes in `.github/release-notes/<tag>.md` with What's Changed and a Full Changelog link; omit test-count boilerplate.
3. Validate, commit, and push the approved annotated tag (`X.Y.Z` or `X.Y.Z-alpha`). Never move an existing release tag or launch duplicate runs.
4. `.github/workflows/release.yml` checks out the tag, requires a non-empty notes file, and publishes that body when creating/updating the release. It builds Linux, Windows, and macOS archives and pushes `ghcr.io/investigenie/investigenie:<tag>` (`latest` for stable releases).
5. Verify the public notes, all three platform archives, and workflow status before declaring the release complete.

## License

By contributing, you agree that your contributions are licensed under the repository [LICENSE](LICENSE).
