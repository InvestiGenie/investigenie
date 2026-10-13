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

- **Security first:** datasources stay read-only. Never return or log secrets. Every limit (size, time, concurrency) stays bounded.
- **No datasource special-casing in shared code:** add the capability to the integration descriptor instead.
- **Errors:** `{"error":{"code","message"}}` with a stable `code`, and no upstream bodies or credentials.
- **Saved objects:** writes use revisions (`If-Match`); keep them that way.
- **UI:** accessible labels and roles (the tests rely on them), and both themes styled.
- **Commits:** imperative subject ≤72 characters, plus a short body explaining what changed and why.

## Where to start

- [Architecture](architecture.md): how the parts fit.
- [Backend](backend.md) and [Frontend](frontend.md): code maps.
- [Integrations](integrations.md): add a datasource (good first contribution).

## Releases (maintainers)

1. Bump the version in `Cargo.toml`, `package.json`, `frontend/package.json`, and the lockfiles.
2. Commit, then publish a GitHub release with tag `X.Y.Z` or `X.Y.Z-alpha` (mark pre-releases).
3. `.github/workflows/release.yml` builds Linux, Windows, and macOS archives and pushes `ghcr.io/investigenie/investigenie:<tag>` (`latest` for stable releases).

## License

By contributing, you agree that your contributions are licensed under the repository [LICENSE](../LICENSE).
