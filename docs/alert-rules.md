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

| Field               | Type | Required | Description                                              |
|---------------------|------|----------|----------------------------------------------------------|
| `threshold_xlm`     | u64  | one of   | Minimum transfer amount in whole XLM (> 0)               |
| `threshold_stroops` | u64  | one of   | Minimum transfer amount in stroops, 1 XLM = 10,000,000 (> 0) |

Exactly one of `threshold_xlm` or `threshold_stroops` must be set. Use `threshold_stroops`
for sub-XLM precision (e.g. `5000000` = 0.5 XLM). Existing `threshold_xlm` configs are unchanged.

Matches when the payment amount (extracted from Horizon operations) is ≥ the threshold.
The `amount_xlm` field in the webhook payload contains the actual transferred amount.

**Note:** Amount is extracted from `payment` operation records. Soroban token transfers
that do not produce a native `payment` operation will not populate `amount_xlm`.

```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000
```

```toml
[[contracts.rules]]
type              = "LargeTransfer"
threshold_stroops = 5000000   # 0.5 XLM
```

### `FunctionCalled`

| Field           | Type   | Required | Description                          |
|-----------------|--------|----------|--------------------------------------|
| `function_name` | string | yes      | Exact function name (case-sensitive) |

Matches when the Soroban `invoke_host_function` operation calls exactly `function_name`.

> **Case sensitivity:** `FunctionCalled` is **case-sensitive**: `function_name = "Withdraw"` never matches a transaction that calls `withdraw`. This is intentional, because Soroban function names are case-sensitive symbols and `Withdraw` and `withdraw` are different functions. `AdminFunctionCalled` is the exception: it is case-insensitive (names are lower-cased in validation and compared in lower case at runtime).

> **Note:** This case-sensitivity asymmetry with `AdminFunctionCalled` is a deliberate design decision, not an oversight. `FunctionCalled` matches the exact symbol as written in the config, while `AdminFunctionCalled` normalises names to lowercase at validation time (in `AlertRule::validate()`). Do not assume a differently-cased `function_name` will match — it silently never fires.
======

```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`

| Field            | Type     | Required | Description                              |
|------------------|----------|----------|------------------------------------------|
| `function_names` | [string] | yes      | Non-empty list of function names to watch (case-insensitive) |

Matches when the invoked function is any entry in `function_names`.
Unlike `FunctionCalled`, this match is **case-insensitive**: every entry is lower-cased
at validation time (in `AlertRule::validate()`) and compared in lower case at runtime, so
`"Set_Admin"`, `"set_admin"` and `"SET_ADMIN"` all match the same function.
Equivalent to multiple `FunctionCalled` rules but produces a single
`AdminFunctionCalled([...])` label in the alert.

> **Note:** This case-insensitivity is a deliberate design decision and is the documented
> exception to `FunctionCalled`'s case-sensitive matching. The asymmetry is intentional.

```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

### `SourceAccountMatch`

| Field            | Type   | Required | Description                                              |
|------------------|--------|----------|----------------------------------------------------------|
| `source_account` | string | yes      | Stellar account ID (`G...`, 56 characters) to match      |

Matches when the transaction's source account equals `source_account`. The value is
validated as a Stellar account ID (`G...`, 56 characters) at config load time.

**Use case:** watch activity from a specific account, e.g. a treasury or admin key.

```toml
[[contracts.rules]]
type           = "SourceAccountMatch"
source_account = "GAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAWHF"
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

### `SuccessRateThreshold`

| Field             | Type | Required | Description                                                        |
|-------------------|------|----------|--------------------------------------------------------------------|
| `min_success_rate`| f64  | yes      | Minimum acceptable success ratio in `[0.0, 1.0]` (e.g. `0.95` = 95%) |
| `window_size`     | u32  | yes      | Number of most recent transactions per contract to consider (> 0)  |

Matches when the success rate over the contract's rolling window of the last `window_size`
transactions falls below `min_success_rate`. The window is tracked per contract and updated
on every observed transaction (successful or failed); the rule does not fire until the
window is full, so a single early failure cannot trigger an alert.

**Use case:** alert when a contract starts failing a meaningful share of its transactions,
e.g. a Soroban contract whose invocations begin reverting.

```toml
[[contracts.rules]]
type             = "SuccessRateThreshold"
min_success_rate = 0.95
window_size      = 100
```

### `TimeWindow`
Restricts alert firing to specific UTC hours and weekdays to prevent alert fatigue outside business hours. Can optionally wrap another rule via `wrap_rule`.

```toml
[[contracts.rules]]
type       = "TimeWindow"
hours_utc  = "09:00-17:00"
weekdays   = ["Mon", "Tue", "Wed", "Thu", "Fri"]
wrap_rule  = "LargeTransfer"
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
| `amount_stroops`      | u64/null    | no             | Transfer amount in stroops if available  |
| `operation_count`     | u32/null    | no             | Total number of operations in the transaction |
| `memo_type`           | string/null | no             | Transaction memo type: `"text"`, `"hash"`, `"id"`, `"return"`, or `"none"` |
| `memo`                | string/null | no             | Transaction memo value                   |
| `ledger_sequence`     | u32/null    | no             | Ledger sequence the transaction was included in |
| `source_account`      | string/null | no             | The `G...` account that signed/submitted the transaction |
| `fee_charged_stroops` | u64/null    | no             | Fee charged for the transaction in stroops |
| `timestamp`           | i64         | yes            | Unix timestamp (seconds) of transaction  |
| `timestamp_iso`       | string      | yes            | ISO 8601 timestamp of the transaction    |
| `horizon_link`        | string      | yes            | Direct link to transaction on Horizon    |
| `explorer_link`       | string      | yes            | Stellar Expert explorer link for the transaction |

> `horizon_link` and `explorer_link` are always present in every alert payload, even when `function_name` is `null` for a non-Soroban transaction.

> **Canonical field list:** the root [`README.md`](../README.md#webhook-payload) "Webhook payload"
> section is the single source of truth for the payload schema. The table above mirrors it; if the
> two ever disagree, trust the README.

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
| `SuccessRateThreshold` | `"SuccessRateThreshold"` |
| `TimeWindow` | `"TimeWindow"` |
