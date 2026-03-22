# Contributing to Seidrum

Thank you for your interest in contributing to Seidrum.

## Getting started

1. Fork the repository
2. Clone your fork
3. Create a feature branch (`git checkout -b feature/my-feature`)
4. Make your changes
5. Run checks: `cargo build --workspace && cargo clippy --workspace && cargo test --workspace`
6. Commit and push to your fork
7. Open a pull request

## Development setup

```bash
# Prerequisites: Rust stable, NATS Server, ArangoDB 3.12+

# Start infrastructure
docker compose up -d nats arangodb

# Configure
cp .env.example .env
# Edit .env with your values

# Build and test
cargo build --workspace
cargo test --workspace

# Initialize database
source .env && target/debug/seidrum-kernel init
```

## Code conventions

- **Logging**: Use `tracing` crate (`info!`, `warn!`, `error!`). Never `println!` in library code.
- **Error handling**: `anyhow::Result` in binaries, `thiserror` for library error types.
- **No `unwrap()`** in production code. Use `?`, `.context()`, or explicit error handling.
- **Event structs** must derive `Serialize, Deserialize, Debug, Clone`.
- **Module naming**: `snake_case` matching the logical component name.
- **Tests**: Write serialization roundtrip tests for event types. Test core logic with unit tests.

## Writing a plugin

Plugins are independent binaries in `crates/plugins/`. Follow the pattern of an existing plugin like `seidrum-code-executor`:

1. Create `crates/plugins/seidrum-my-plugin/` with `Cargo.toml` and `src/main.rs`
2. Add the crate to the workspace `members` in the root `Cargo.toml`
3. Register the plugin on startup via `plugin.register`
4. Subscribe to NATS subjects and process events
5. Optionally register capabilities via `capability.register`

See [docs/PLUGIN_SPEC.md](docs/PLUGIN_SPEC.md) for the full specification.

## Pull requests

- Keep PRs focused on a single change
- Include tests for new functionality
- Make sure `cargo clippy --workspace` passes with no new warnings
- Write a clear description of what changed and why

## Reporting issues

Open an issue on GitHub with:
- What you expected to happen
- What actually happened
- Steps to reproduce
- Relevant logs (with secrets redacted)

## License

By contributing, you agree that your contributions will be licensed under the [MIT License](LICENSE).
