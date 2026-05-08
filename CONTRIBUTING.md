# Contributing to stellar-txwatch-core

## Dev setup

```bash
git clone https://github.com/Veritas-Vaults-Network/stellar-txwatch-core
cd stellar-txwatch-core
cargo build
```

Requires Rust stable ≥ 1.75.

## Running locally

```bash
# Validate your config
cargo run -p txwatch -- --config config/example.toml validate

# List contracts
cargo run -p txwatch -- --config config/example.toml list-contracts

# Start the polling engine
cargo run -p txwatch -- --config config/example.toml start
```

## Crate map

| Crate               | Responsibility                                      |
|---------------------|-----------------------------------------------------|
| `txwatch-config`    | TOML parsing, `AppConfig`, `WatchedContract`, types |
| `txwatch-rules`     | `HorizonTransaction`, `AlertPayload`, rule eval     |
| `txwatch-notifier`  | Webhook HTTP delivery with exponential-backoff retry|
| `txwatch-poller`    | Horizon polling loop, cursor tracking, orchestration|
| `txwatch` (cli)     | `clap` CLI binary — `start`, `validate`, `list-contracts` |

## Adding a new rule type

1. Add a variant to `AlertRule` in `crates/config/src/lib.rs`.
2. Add the match arm in `evaluate()` in `crates/rules/src/lib.rs`.
3. Add the label string in `rule_label()` in the same file.
4. Document it in `docs/alert-rules.md`.

## PR guidelines

- Keep PRs focused on a single concern.
- Run `cargo clippy -- -D warnings` and `cargo fmt` before opening a PR.
- Add an entry to the relevant doc file for any user-visible change.
