# Alert Rules Reference

Rules are evaluated per-transaction for each watched contract.
Multiple rules can match the same transaction — each fires an independent webhook call.
A rule evaluation error is logged as a warning and skipped; it never stops the engine.

## Rule types

### `AnyTransaction`
Matches every transaction that appears in the contract's Horizon history.

**Use case:** full audit trail, low-volume contracts.

```toml
[[contracts.rules]]
type = "AnyTransaction"
```

### `TransactionFailed`
Matches transactions where `successful = false`.

**Use case:** detect reverted Soroban invocations or fee-bump failures.

```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### `SorobanInvocation`
Matches any transaction that invokes at least one Soroban host function
(`invoke_host_function`), whichever function is called. Native-only transactions such as
payments or offers do not match.

**Use case:** monitor all smart-contract activity on a contract without listing function names.
Use `FunctionCalled` or `AdminFunctionCalled` when you only care about specific functions.

```toml
[[contracts.rules]]
type = "SorobanInvocation"
```

### `InsufficientFunds`
Matches transactions that failed because the source account could not cover the fee
(Horizon result code `txINSUFFICIENT_BALANCE`, decoded from `result_xdr`). Other failures
such as `txBAD_SEQ` or `txFAILED` do not match, and successful transactions never match.

**Use case:** catch fee-account top-up problems before they cause a run of failed transactions.

```toml
[[contracts.rules]]
type = "InsufficientFunds"
```

### `LargeTransfer`

| Field           | Type | Required | Description                        |
|-----------------|------|----------|------------------------------------|
| `threshold_xlm` | u64  | yes      | Minimum transfer amount in XLM (> 0) |

Matches when the payment amount (extracted from Horizon operations) is ≥ `threshold_xlm` XLM.
The `amount_xlm` field in the webhook payload contains the actual transferred amount.

**Note:** Amount is extracted from `payment` operation records. Soroban token transfers
that do not produce a native `payment` operation will not populate `amount_xlm`.

```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000
```

### `FunctionCalled`

| Field           | Type   | Required | Description                          |
|-----------------|--------|----------|--------------------------------------|
| `function_name` | string | yes      | Exact function name (case-sensitive) |

Matches when the Soroban `invoke_host_function` operation calls exactly `function_name`.

> **Case sensitivity:** `FunctionCalled` is **case-sensitive**: `function_name = "Withdraw"` never matches a transaction that calls `withdraw`. This is intentional, because Soroban function names are case-sensitive symbols and `Withdraw` and `withdraw` are different functions. `AdminFunctionCalled` is the exception: it is case-insensitive (names are lower-cased in validation and compared in lower case at runtime).

```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`

| Field            | Type     | Required | Description                              |
|------------------|----------|----------|------------------------------------------|
| `function_names` | [string] | yes      | Non-empty list of function names to watch |

Matches when the invoked function is any entry in `function_names`.
Unlike `FunctionCalled`, this match is **case-insensitive**.
Equivalent to multiple `FunctionCalled` rules but produces a single
`AdminFunctionCalled([...])` label in the alert.

```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

### `HighFee`

| Field                | Type | Required | Description                                                   |
|----------------------|------|----------|---------------------------------------------------------------|
| `threshold_stroops`  | u64  | one of   | Fee threshold in stroops (> 0)                                |
| `threshold_xlm`      | u64  | one of   | Fee threshold in whole XLM (> 0); converted to stroops at validation time |

Exactly one of `threshold_stroops` or `threshold_xlm` must be set. Setting both is a config error.

Matches when the transaction's total fee exceeds the threshold.
The `fee_charged_stroops` field in the webhook payload contains the actual fee paid in stroops.

**Note:** Stroops are the smallest unit of XLM (1 XLM = 10,000,000 stroops).
`threshold_xlm = 1` is equivalent to `threshold_stroops = 10000000`.

```toml
[[contracts.rules]]
type               = "HighFee"
threshold_stroops  = 100000
```

```toml
[[contracts.rules]]
type           = "HighFee"
threshold_xlm  = 1
```

## Evaluation order

Rules are evaluated in the order they appear in the config file.
All matching rules fire; there is no short-circuit.

## Webhook payload fields

| Field                 | Type        | Always present | Description                              |
|-----------------------|-------------|----------------|------------------------------------------|
| `label`               | string      | yes            | Contract label from config               |
| `contract_id`         | string      | yes            | Stellar C-address                        |
| `network`             | string      | yes            | `mainnet` / `testnet` / `futurenet`      |
| `rule_type`           | string      | yes            | Stable machine-readable rule variant (see [Stable rule_type values](#stable-rule_type-values)) |
| `rule_triggered`      | string      | yes            | Human-readable rule description          |
| `transaction_hash`    | string      | yes            | Stellar transaction hash                 |
| `function_name`       | string/null | no             | First invoked Soroban function name if available; `null` indicates a non-Soroban transaction |
| `function_names`      | [string]    | yes            | All invoked Soroban function names in the transaction (may contain multiple entries) |
| `amount_xlm`          | u64/null    | no             | Transfer amount in XLM if available      |
| `fee_charged_stroops` | u64/null    | no             | Fee charged for the transaction in stroops |
| `timestamp`           | i64         | yes            | Unix timestamp (seconds) of transaction  |
| `timestamp_iso`       | string      | yes            | ISO 8601 timestamp of the transaction    |
| `horizon_link`        | string      | yes            | Direct link to transaction on Horizon    |
| `explorer_link`       | string      | yes            | Stellar Expert explorer link for the transaction |

> `horizon_link` and `explorer_link` are always present in every alert payload, even when `function_name` is `null` for a non-Soroban transaction.

## Stable rule_type values

The webhook payload includes two rule-related fields:

| Field | Purpose | Example |
|-------|---------|---------|
| `rule_type` | Machine-readable, stable rule variant name; use for programmatic routing | `"LargeTransfer"` |
| `rule_triggered` | Human-readable description with parameters; use for display | `"LargeTransfer(>=10000XLM)"` |

### Rule type table

| Rule | `rule_type` value |
|------|-------------------|
| `AnyTransaction` | `"AnyTransaction"` |
| `TransactionFailed` | `"TransactionFailed"` |
| `InsufficientFunds` | `"InsufficientFunds"` |
| `SorobanInvocation` | `"SorobanInvocation"` |
| `LargeTransfer` | `"LargeTransfer"` |
| `FunctionCalled` | `"FunctionCalled"` |
| `AdminFunctionCalled` | `"AdminFunctionCalled"` |
| `HighFee` | `"HighFee"` |
| `SourceAccountMatch` | `"SourceAccountMatch"` |

## Adding a new rule type

1. Add a variant to `AlertRule` in `crates/config/src/lib.rs`
2. Add field validation in `AlertRule::validate()` in the same file
3. Add a match arm in `AlertRule::label()` in the same file (human-readable description)
4. Add a match arm in `AlertRule::rule_type()` in the same file (stable machine-readable name)
5. Add the match arm in `eval_rule()` in `crates/rules/src/lib.rs`
6. Add unit tests in `crates/rules/src/lib.rs`
7. Update the rule type table in this section
8. Update the webhook payload example in README.md (if adding a new example)

No other crates need changes.
