# Configuration Reference

Config is a TOML file passed via `--config` (default: `config/example.toml`).

## Top-level fields

| Field                   | Type | Required | Description                           |
|-------------------------|------|----------|---------------------------------------|
| `poll_interval_seconds` | u64  | yes      | How often to poll Horizon (seconds). Must be > 0. |

## `[[contracts]]`

Each entry defines one watched Soroban contract. At least one entry is required.

| Field         | Type   | Required | Description                                                  |
|---------------|--------|----------|--------------------------------------------------------------|
| `label`       | string | yes      | Human-readable name shown in logs and alert payloads.        |
| `contract_id` | string | yes      | Stellar C-address (56 chars, starts with `C`).               |
| `network`     | string | yes      | `mainnet`, `testnet`, or `futurenet`.                        |
| `webhook_url` | string | yes      | `http://` or `https://` endpoint that receives alert JSON.   |
| `webhook_secret` | string | no    | If set, sent as `X-TxWatch-Secret` header on every POST.     |

## `[[contracts.rules]]`

At least one rule is required per contract. All matching rules fire independently.

### `AnyTransaction`
Fires on every transaction that appears in the contract's Horizon history.

```toml
[[contracts.rules]]
type = "AnyTransaction"
```

### `TransactionFailed`
Fires when `successful = false`.

```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### `LargeTransfer`
Fires when the payment amount ≥ `threshold_xlm` XLM.

```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000          # must be > 0
```

### `FunctionCalled`
Fires when the Soroban invocation calls exactly `function_name` (case-sensitive).

```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`
Fires when the invoked function is any entry in `function_names`.

```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

## Webhook payload

```json
{
  "label":            "My Escrow Contract",
  "contract_id":      "CAAA...",
  "network":          "testnet",
  "rule_triggered":   "LargeTransfer(>=10000XLM)",
  "transaction_hash": "abc123...",
  "function_name":    "transfer",
  "amount_xlm":       15000,
  "timestamp":        1705316096,
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123..."
}
```

## Environment variables

| Variable   | Default | Description                                      |
|------------|---------|--------------------------------------------------|
| `RUST_LOG` | `info`  | Log level: `error`, `warn`, `info`, `debug`, `trace` |

## Full example

```toml
poll_interval_seconds = 10

[[contracts]]
label       = "My Escrow Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/my-webhook"

  [[contracts.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 10000

  [[contracts.rules]]
  type           = "AdminFunctionCalled"
  function_names = ["set_admin", "upgrade", "initialize"]

  [[contracts.rules]]
  type = "TransactionFailed"
```
