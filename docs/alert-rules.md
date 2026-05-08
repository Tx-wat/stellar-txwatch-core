# Alert Rules Reference

Rules are evaluated per-transaction for each watched contract. Multiple rules
can match the same transaction — each match fires an independent webhook call.

## Rule types

### `AnyTransaction`
Matches every transaction that appears in the contract's Horizon history.
Use this for full audit trails or low-noise contracts.

### `TransactionFailed`
Matches transactions where `successful = false`. Useful for detecting
reverted invocations or fee-bump failures.

### `LargeTransfer`
```toml
type          = "LargeTransfer"
threshold_xlm = 10000        # inclusive lower bound
```
Matches when the transferred amount (in XLM) is ≥ `threshold_xlm`.
The `amount` field in the webhook payload contains the XLM value.

### `FunctionCalled`
```toml
type          = "FunctionCalled"
function_name = "withdraw"
```
Matches when the Soroban invocation calls exactly `function_name`.
Case-sensitive.

### `AdminFunctionCalled`
```toml
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```
Matches when the invoked function is any entry in `function_names`.
Equivalent to multiple `FunctionCalled` rules but produces a single
`AdminFunctionCalled(...)` label in the alert.

## Evaluation order
Rules are evaluated in the order they appear in the config file.
All matching rules fire; there is no short-circuit.

## Extending rules
Add a new variant to `AlertRule` in `crates/config/src/lib.rs`, then add
the corresponding match arm in `crates/rules/src/lib.rs` → `evaluate()`.
No other crates need changes.
