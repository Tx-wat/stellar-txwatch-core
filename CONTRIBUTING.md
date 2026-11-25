# Contributing to stellar-txwatch-core

## Local Development with Docker Compose

### Prerequisites

- Docker and Docker Compose installed

### Getting started

Start the full stack (txwatch + webhook echo server):

```bash
docker compose up
```

This runs:
- **txwatch** — the main polling service (configured to watch the example contract)
- **webhook** — a local echo server listening on `http://localhost:8080` that logs all incoming POST requests

### Viewing webhook payloads

All webhook calls from txwatch are logged by the echo server. Watch the output in your terminal:

```
webhook   | {"timestamp":"2025-02-28T...", "method":"POST", "url":"/webhook", "body":{...}}
```

You can also inspect payloads by manually curling the webhook:

```bash
curl -X POST http://localhost:8080/webhook -H "Content-Type: application/json" -d '{"test": "payload"}'
```

### Editing the config

To watch a different contract or change alert rules:

1. Edit `config/example.toml`
2. Restart the stack: `docker compose down && docker compose up`

### Stopping the stack

```bash
docker compose down
```

---

## Sister repos

| Repo | Description |
|------|-------------|
| [stellar-txwatch-web](https://github.com/Veritas-Vaults-Network/stellar-txwatch-web) | Web dashboard for alert history |
| [stellar-txwatch-contracts](https://github.com/Veritas-Vaults-Network/stellar-txwatch-contracts) | Example Soroban contracts to monitor |

---

## Devcontainer / one-click development environment

The repository ships a [`.devcontainer/devcontainer.json`](.devcontainer/devcontainer.json)
configuration so you can get a fully working Rust development environment
without installing anything locally.

### Opening the devcontainer

- **GitHub Codespaces:** click *Code → Codespaces → Create codespace on main*
  on the repository page. The container is built and configured automatically.
- **VS Code (local):** install the
  [Dev Containers extension](https://marketplace.visualstudio.com/items?itemName=ms-vscode-remote.remote-containers),
  open the repository folder, and run *Dev Containers: Reopen in Container*
  from the command palette (`Ctrl+Shift+P` / `Cmd+Shift+P`).

The container is based on the official `rust:latest` image.

### What it installs

- **Rust toolchain** (stable, from the base image)
- **clippy** and **rustfmt** components (installed via the post-create command
  `rustup component add clippy rustfmt`)
- **VS Code extensions:**
  - [`rust-analyzer`](https://marketplace.visualstudio.com/items?itemName=rust-lang.rust-analyzer) — language server, completion, inline errors
  - [`vadimcn.vscode-lldb`](https://marketplace.visualstudio.com/items?itemName=vadimcn.vscode-lldb) (CodeLLDB) — native debugger
  - [`tamasfe.even-better-toml`](https://marketplace.visualstudio.com/items?itemName=tamasfe.even-better-toml) — TOML syntax highlighting and validation for config files

### Building and testing inside the container

Open a terminal in the container (VS Code: *Terminal → New Terminal*) and run:

```bash
cargo build --workspace
cargo test --workspace
cargo clippy -- -D warnings
cargo fmt --check
```

These are the same commands CI runs, so a green run locally means a green PR.

---

## Local dev setup

### 1. Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
rustup update stable
```

Requires Rust stable ≥ 1.75.

### 2. Clone and build

```bash
git clone https://github.com/Veritas-Vaults-Network/stellar-txwatch-core
cd stellar-txwatch-core
cargo build
```

### 3. Validate the example config

```bash
cargo run -p txwatch -- --config config/example.toml validate
```

### 4. Get a testnet contract to watch

The easiest way is to deploy one of the contracts from
[stellar-txwatch-contracts](https://github.com/Veritas-Vaults-Network/stellar-txwatch-contracts),
or use any existing Soroban contract on Testnet.

You can find active testnet contracts on the
[Stellar Expert Testnet explorer](https://stellar.expert/explorer/testnet).

Copy the contract's C-address (56 characters, starts with `C`) into your config:

```toml
[[contracts]]
label       = "My Test Contract"
contract_id = "CXXX..."   # paste your contract address here
network     = "testnet"
webhook_url = "https://webhook.site/your-unique-id"  # use webhook.site for testing
```

### 5. Start watching

```bash
RUST_LOG=debug cargo run -p txwatch -- --config config/my-config.toml watch
```

---

## Running Tests

```bash
cargo test --workspace
```

This runs all tests across all crates. For testing a specific crate:

```bash
cargo test -p txwatch-config
cargo test -p txwatch-rules
cargo test -p txwatch-notifier
cargo test -p txwatch-poller   # includes integration tests
```

Add `-- --nocapture` to see log output while tests run.

## Configuration schema

`config/schema.json` is generated from the configuration types with `schemars`.
If you change configuration fields, Serde attributes, or schema descriptions,
regenerate this file from the repository root:

```bash
cargo run --locked --quiet -p txwatch-config --example export_schema > config/schema.json
```

The example uses the same `AppConfig::json_schema()` helper as `txwatch schema`.
It builds only the configuration crate. Do not edit the generated JSON by hand.

Run the focused schema tests:

```bash
cargo test --locked -p txwatch-config --test schema
```

CI runs these tests in the `config-schema` job. The snapshot test fails if the
generated schema differs from `config/schema.json`. Review the schema changes and
include the regenerated file in your pull request.

Run the command tests when the CLI builds:

```bash
cargo test --locked -p txwatch --test schema
```

## Dependency checks

CI runs [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) on every pull
request. It checks advisories (RustSec), licenses, banned crates, and crate
sources against [`deny.toml`](deny.toml). Run it locally before submitting a PR
when you add or update dependencies:

```bash
cargo install cargo-deny --locked   # once
cargo deny check
```

## Adding Tests

### Where to put tests

- **Unit tests:** in the same file as the code under test, in a `#[cfg(test)]` module at the bottom
- **Integration tests:** in a `tests/` directory at the crate root

### Using wiremock

The project uses [wiremock](https://crates.io/crates/wiremock) to mock HTTP endpoints in integration tests — it spins up local HTTP servers that simulate Horizon and webhook endpoints without requiring network access. See `crates/poller/tests/integration.rs` for examples of how to use it.

---

## Dependency lock files

`Cargo.lock` is committed to this repository. This workspace contains both
a binary crate (`crates/cli`) and library crates, and we commit the lock
file to ensure reproducible builds of the binary.

**When submitting a pull request:**

- If you update dependencies (directly or indirectly), commit the updated `Cargo.lock`
- Use `cargo update` to update dependencies, then commit the resulting lock file
- Do not remove or exclude `Cargo.lock` from commits

This ensures all contributors and CI builds use the exact same dependency versions.

---

## How to add a new `AlertRule` type

Adding a new alert rule touches the config crate (the enum, validation, and
label/type metadata) and the rules crate (evaluation). Follow these steps in
order. Each step is in a different file.

### Step 1 — Declare the variant (`crates/config/src/lib.rs`)

Add your variant to the `AlertRule` enum. Use `#[serde(rename_all = "snake_case")]`
style naming consistent with the existing variants (e.g. `FunctionCall`,
`LargeTransfer`, `FailedTransaction`):

```rust
pub enum AlertRule {
    // ... existing variants ...
    MyNewRule { my_field: String },
}
```

### Step 2 — Validate the new fields (`crates/config/src/lib.rs`)

Add a match arm in `AlertRule::validate()`. Validation runs at startup via
`txwatch --config <file> validate`, so reject bad config early with a clear
message that names the contract label:

```rust
AlertRule::MyNewRule { my_field } => {
    if my_field.trim().is_empty() {
        bail!("contract '{}': MyNewRule my_field must not be empty", contract_label);
    }
}
```

### Step 3 — Provide `label()` and `rule_type()` (`crates/config/src/lib.rs`)

Every variant must return a human-readable `label()` (used in webhook payloads
and logs) and a stable `rule_type()` string (used for filtering and the
`wrap_rule` matcher). Keep `rule_type()` values stable — they are part of the
public config/notification surface:

```rust
impl AlertRule {
    pub fn label(&self) -> String {
        match self {
            // ... existing arms ...
            AlertRule::MyNewRule { my_field } => format!("MyNewRule({my_field})"),
        }
    }

    pub fn rule_type(&self) -> &'static str {
        match self {
            // ... existing arms ...
            AlertRule::MyNewRule { .. } => "MyNewRule",
        }
    }
}
```

### Step 4 — Evaluate the rule (`crates/rules/src/lib.rs`)

Add a match arm in `eval_rule()` that returns `Ok(true)` when the transaction
matches:

```rust
AlertRule::MyNewRule { my_field } => {
    Ok(tx.function_name.as_deref() == Some(my_field.as_str()))
}
```

> Note: Adding a new `AlertRule` variant requires updating both `eval_rule()` and
> `AlertRule::label()` / `rule_type()` in `crates/config/src/lib.rs`. Rust's
> exhaustive matching helps catch missing arms, but these functions must remain
> in sync for new variants.

### Step 5 — Handle the `wrap_rule` matcher (`crates/rules/src/lib.rs`)

`eval_rule()` is **not** the only evaluation path. `eval_time_window()` also
matches on the wrapped rule's type name via `TimeWindow.wrap_rule`
(`Option<String>`), in a separate `match` on `rule_type()` strings. That match
has a `_ => Ok(true)` catch-all, so a new rule type that is not handled there
will **silently always fire** for every transaction inside the time window
(see issue #453).

For every new variant, add a matching arm to the `wrap_rule` match in
`eval_time_window()` that delegates to the same logic as `eval_rule()`:

```rust
match wrapped {
    // ... existing arms ...
    "MyNewRule" => Ok(/* the same predicate as eval_rule */),
    _ => Ok(true),
}
```

### Step 6 — Add tests

Add unit tests next to the code under test:

- **Config tests** (`crates/config/src/lib.rs` or `crates/config/tests/`):
  deserialize a TOML snippet containing the new variant, assert `validate()`
  accepts valid input and rejects invalid input, and assert `label()` /
  `rule_type()` return the expected values.
- **Rule tests** (`crates/rules/src/lib.rs`): build a `Transaction` that should
  match and one that should not, and assert `eval_rule()` returns the expected
  result. If the rule can be wrapped in a `TimeWindow`, add a test covering the
  `wrap_rule` path too.

Run the relevant crates before opening a PR:

```bash
cargo test -p txwatch-config
cargo test -p txwatch-rules
```

### Reference examples

Use the existing rules as templates — they cover the common shapes:

| Rule | Shape |
|------|-------|
| `FunctionCall` | matches on `function_name` |
| `LargeTransfer` | matches on amount threshold |
| `FailedTransaction` | matches on transaction status |

---

## Checklist: adding a new `AlertRule`

Copy this into your PR description and tick each item:

- [ ] Added the variant to the `AlertRule` enum in `crates/config/src/lib.rs`
- [ ] Added a validation arm in `AlertRule::validate()` with a clear error message
- [ ] Updated `AlertRule::label()` for the new variant
- [ ] Updated `AlertRule::rule_type()` with a stable string
- [ ] Added an `eval_rule()` match arm in `crates/rules/src/lib.rs`
- [ ] Added a `wrap_rule` arm in `eval_time_window()` (or confirmed it is not wrappable)
- [ ] Added config tests (deserialize, validate, label/rule_type)
- [ ] Added rule tests (matching and non-matching transactions)
- [ ] Updated `config/example.toml` if the rule is worth demonstrating
- [ ] `cargo test --workspace` and `cargo clippy -- -D warnings` pass
