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

## Before you open a PR

```sh
cargo fmt --all
cargo test -p investigenie-api
npm run test:web
npm run build:web
```

- Add or update tests for every behaviour change. Backend upstreams are mocked with `wiremock`; frontend tests use Vitest + Testing Library.
- Only commit files you changed (if `cargo fmt` reformats other files, leave those out).
- Update the docs in `docs/` or `README.md` when behaviour, configuration, or APIs change.

## Conventions

- Shared connector contracts and registration belong in `backend/src/data_connectors.rs` and `backend/src/ai_connectors.rs`; keep concrete implementations under `integrations/data_connectors/<id>/` and `integrations/ai_connectors/<id>/`.
- Data connectors import `crate::data_connectors` and are auto-discovered by `backend/build.rs`. AI transports implement `crate::ai_connectors::AiConnector` and are explicitly registered in the backend AI contract; do not mix the two registries.

- **Security first:** datasources stay read-only. Never return or log secrets. Every limit (size, time, concurrency) stays bounded.
- **No datasource special-casing in shared code:** add the capability to the integration descriptor instead.
- **Errors:** `{"error":{"code","message"}}` with a stable `code`, and no upstream bodies or credentials.
- **Saved objects:** writes use revisions (`If-Match`); keep them that way.
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
