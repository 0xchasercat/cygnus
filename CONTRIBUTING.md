# Contributing

## Prerequisites

- **Rust stable** — the version is pinned in `rust-toolchain.toml`; `rustup` picks it up automatically.
- **Bun** — required to build and test the web console (`console/`).

## Build

```sh
cargo build --workspace          # daemon, CLI, init
cd console && bun install && bun run build   # dashboard
```

## Tests

```sh
cargo test --workspace           # unit + integration tests
cd console && bun install && bun test        # console tests
```

Some cage integration tests require Linux and root (they exercise the full
namespace and cgroup stack). Those tests detect when they are running on
macOS or without sufficient privileges and skip themselves automatically.

## Code style

```sh
cargo fmt                        # format — required before committing
cargo clippy -- -D warnings      # CI enforces zero warnings
```

The CI pipeline runs `fmt --check` and `clippy -D warnings` on every pull
request. Fix any warnings before submitting.

## Pull requests

- Keep PRs small and focused on a single concern.
- Include tests for any behavior change or new feature.
- Update relevant documentation in `docs/` when the user-visible behavior changes.
- The PR description should explain why the change is needed, not just what it does.

## Discussing changes

Open a GitHub issue before starting significant work. This avoids duplicated
effort and gives a place to agree on the approach before implementation.

## Community guidelines

This project follows [GitHub's community guidelines](https://docs.github.com/en/site-policy/github-terms/github-community-guidelines).
