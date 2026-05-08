# Contributing to stellar-txwatch-core

## Sister repos

| Repo | Description |
|------|-------------|
| [stellar-txwatch-web](https://github.com/Veritas-Vaults-Network/stellar-txwatch-web) | Web dashboard for alert history |
| [stellar-txwatch-contracts](https://github.com/Veritas-Vaults-Network/stellar-txwatch-contracts) | Example Soroban contracts to monitor |

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

## Running tests

```bash
# All tests across all crates
cargo test

# Single crate
cargo test -p txwatch-config
cargo test -p txwatch-rules
cargo test -p txwatch-notifier
cargo test -p txwatch-poller   # includes integration tests

# With log output visible
cargo test -- --nocapture
```

### Integration tests

Integration tests in `crates/poller/tests/integration.rs` use
[wiremock](https://crates.io/crates/wiremock) to spin up local HTTP servers
that simulate Horizon and webhook endpoints. No network access is required.

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

### Step 4 — Label the rule (`crates/rules/src/lib.rs`)

Add a match arm in `rule_label()`:

```rust
AlertRule::MyNewRule { my_field } => format!("MyNewRule({})", my_field),
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

Add an entry to `docs/alert-rules.md`.

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
- [ ] New rule documented in `docs/alert-rules.md`
- [ ] CONTRIBUTING.md updated if the contribution process changed
