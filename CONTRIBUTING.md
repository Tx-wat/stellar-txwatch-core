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

Follow these steps in order. Each step is in a different file.

### Step 1 — Declare the variant (`crates/config/src/lib.rs`)

Add your variant to the `AlertRule` enum:

```rust
pub enum AlertRule {
    // ... existing variants ...
    MyNewRule { my_field: String },
}
```

### Step 2 — Validate the new fields (`crates/config/src/lib.rs`)

Add a match arm in `AlertRule::validate()`:

```rust
AlertRule::MyNewRule { my_field } => {
    if my_field.trim().is_empty() {
        bail!("contract '{}': MyNewRule my_field must not be empty", contract_label);
    }
}
```

### Step 3 — Evaluate the rule (`crates/rules/src/lib.rs`)

Add a match arm in `eval_rule()`:

```rust
AlertRule::MyNewRule { my_field } => {
    Ok(tx.function_name.as_deref() == Some(my_field.as_str()))
}
```

> Note: Adding a new `AlertRule` variant requires updating both `eval_rule()` and
> `AlertRule::label()` in `crates/config/src/lib.rs`. Rust's exhaustive matching helps
> catch missing arms, but both functions must remain in sync for new variants.

### Step 3b — Handle the `wrap_rule` matcher (`crates/rules/src/lib.rs`)

`eval_rule()` is **not** the only evaluation path. `eval_time_window()` also
matches on the wrapped rule's type name via `TimeWindow.wrap_rule`
(`Option<String>`), in a separate `match` on `rule_type()` strings. That match
has a `_ => Ok(true)` catch-all, so a new rule type that is not handled there
will **silently always fire** for every transaction inside the time window
(see issue #453).

For every new variant, do one of the following:

1. **Add a matching arm** to the `wrap_rule` match in `eval_time_window()`:

   ```rust
   match wrapped {
       // ... existing arms ...
       "MyNewRule" => Ok(/* the same condition as eval_rule() */),
       _ => Ok(true),
   }
   ```

   Note that `wrap_rule` only carries a bare type name, so if the rule's
   condition depends on its own parameters (e.g. a function name), you cannot
   express it here without threading those parameters through — in that case
   use option 2.

2. **Reject it at config-validation time**: add the rule type to the
   `TimeWindow` arm of `AlertRule::validate()` in `crates/config/src/lib.rs`
   so that configuring it as a `wrap_rule` value fails with a clear error
   instead of falling through to always-true:

   ```rust
   AlertRule::TimeWindow { wrap_rule, .. } => {
       if let Some(wrapped) = wrap_rule {
           if !SUPPORTED_WRAP_RULES.contains(&wrapped.as_str()) {
               bail!("TimeWindow wrap_rule '{}' is not supported", wrapped);
           }
       }
   }
   ```

Whichever option you choose, add a test covering the `wrap_rule` path for the
new rule type.

### Step 4 — Label the rule (`crates/config/src/lib.rs`)

Add a match arm in `AlertRule::label()` and `AlertRule::rule_type()`:

```rust
// In label():
AlertRule::MyNewRule { my_field } => format!("MyNewRule({})", my_field),

// In rule_type():
AlertRule::MyNewRule { .. } => "MyNewRule".into(),
```

### Step 5 — Add to the TOML config format

Users configure it like:

```toml
[[contracts.rules]]
type     = "MyNewRule"
my_field = "some_value"
```

### Step 6 — Write tests

Add unit tests in `crates/rules/src/lib.rs` and optionally an integration
test in `crates/poller/tests/integration.rs`.

### Step 7 — Document it

`docs/alert-rules.md` has two distinct places that must be updated for every
new rule. Do both:

1. **Add a `### RuleName` section to `docs/alert-rules.md`** describing the
   rule, its fields, and a TOML example:

   ````markdown
   ### MyNewRule

   Fires when ... .

   | Field | Type | Required | Description |
   |-------|------|----------|-------------|
   | `my_field` | string | yes | ... |

   ```toml
   [[contracts.rules]]
   type     = "MyNewRule"
   my_field = "some_value"
   ```
   ````

2. **Add a row to the "Stable rule_type values" → Rule type table in
   `docs/alert-rules.md`** mapping the rule name to its `rule_type` string
   (the value returned by `AlertRule::rule_type()`):

   ```markdown
   | `MyNewRule` | `MyNewRule` |
   ```
=======

## Architecture Decision Records (ADRs)

Key architectural decisions are documented in `docs/adr/`:
- [ADR 001: Advance Cursor Before Transaction Enrichment](docs/adr/001-cursor-advance-before-enrichment.md)

---

## Code style

```bash
cargo fmt          # format
cargo clippy -- -D warnings   # lint (must pass clean)
```

## Commit style

Use [Conventional Commits](https://www.conventionalcommits.org/):

```
feat(rules): add MyNewRule evaluation
fix(config): reject empty contract labels
test(poller): add integration test for MyNewRule
docs: document MyNewRule in alert-rules.md
```

## PR checklist

- [ ] `cargo test` passes
- [ ] `cargo clippy -- -D warnings` passes
- [ ] `cargo fmt` applied
- [ ] New rule documented in `docs/alert-rules.md`: a `### RuleName` prose section **and** a row in the "Stable rule_type values" → Rule type table
- [ ] CONTRIBUTING.md updated if the contribution process changed
- [ ] CHANGELOG.md entry added under `## [Unreleased]` in the matching section: `### Added`, `### Changed`, `### Fixed` or `### Removed` (see [Keep a Changelog](https://keepachangelog.com/en/1.0.0/))

---

## Releasing

Releases are cut by pushing a version tag. The `Release` workflow
(`.github/workflows/release.yml`) then runs the test suite, builds the Linux
x86_64 `txwatch` binary, and creates a GitHub Release.

1. Move the entries under `## [Unreleased]` in `CHANGELOG.md` into a new
   `## [X.Y.Z] - YYYY-MM-DD` section, and leave `## [Unreleased]` empty.
2. Merge that change to `main`.
3. Tag the commit and push the tag:

   ```bash
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

The release notes are the body of the `## [X.Y.Z]` section. The workflow fails
if that section is missing or empty, so fill it in before tagging.
