# Configuration Reference

Config is a TOML file passed via `--config` (default: `config/example.toml`).

## Top-level fields

| Field                   | Type | Required | Description                           |
|-------------------------|------|----------|---------------------------------------|
| `poll_interval_seconds` | u64  | yes      | How often to poll Horizon (seconds). Must be >= 5 and ≤ 3600. |

> **Horizon rate limits:** Polling too frequently across many contracts can exhaust Horizon's per-IP request quota,
> resulting in `429 Too Many Requests` responses. Sustained polling across six or more contracts at intervals below
> 10 seconds is known to trigger rate limiting in production. The recommended minimum is
> `poll_interval_seconds = 10`; for high-volume deployments with many contracts, `poll_interval_seconds = 30` or
> higher is advised. TxWatch logs a startup warning when `poll_interval_seconds < 10` and more than 5 contracts
> are configured.

> **Contract limit:** A maximum of **100** `[[contracts]]` entries are allowed per configuration file. Exceeding this limit is rejected at startup to prevent exhausting memory or file descriptors from too many concurrent Horizon polling tasks.

## `[[contracts]]`

Each entry defines one watched Soroban contract. At least one entry is required.

| Field         | Type   | Required | Description                                                  |
|---------------|--------|----------|--------------------------------------------------------------|
| `label`       | string | yes      | Human-readable name shown in logs and alert payloads.        |
| `contract_id` | string | yes      | Stellar C-address (56 chars, starts with `C`).               |
| `network`     | string | yes      | `mainnet`, `testnet`, or `futurenet`.                        |
| `webhook_url` | string | yes      | `http://` or `https://` endpoint that receives alert JSON.   |
| `webhook_secret` | string | no    | If set, an HMAC-SHA256 signature of the request body is sent as `X-TxWatch-Signature: sha256=<hmac>`. Never sends the raw secret over the wire. |

### Network field values

Valid `network` values and their corresponding Horizon endpoints:

| Value | Horizon URL |
|---|---|
| `mainnet` | https://horizon.stellar.org |
| `testnet` | https://horizon-testnet.stellar.org |
| `futurenet` | https://horizon-futurenet.stellar.org |

Any value outside this list will cause a TOML parse error. For example:

```
Error: unknown variant `main`, expected one of `mainnet`, `testnet`, `futurenet`
```

To fix: replace your `network` value with one of the valid values listed above.

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
  "function_names":   ["transfer"],
  "amount_xlm":       15000,
  "timestamp":        1705316096,
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":    "https://stellar.expert/explorer/testnet/tx/abc123..."
}
```

- `function_name` — the first invoked Soroban function name (present for backward compatibility).
- `function_names` — all Soroban function names invoked in the transaction (one per `invoke_host_function` operation). Most transactions have zero or one entry.

## Environment variables

| Variable         | Default               | Description                                                                 |
|------------------|-----------------------|-----------------------------------------------------------------------------|
| `RUST_LOG`       | `info`                | Log level: `error`, `warn`, `info`, `debug`, `trace`                        |
| `TXWATCH_CONFIG` | `config/example.toml` | Path to the TOML config file. Used when `--config` is not passed; the `--config` flag takes precedence. |

Note: setting `RUST_LOG=debug` will show per-contract idle poll cycles — the
poller emits `"no new transactions"` debug logs with the contract `label` and
current `cursor` when a poll returns an empty page.

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
