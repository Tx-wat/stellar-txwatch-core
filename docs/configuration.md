# Configuration Reference

Config is a TOML file passed via `--config` (default: `config/example.toml`).

## Top-level fields

| Field                   | Type | Required | Description                           |
|-------------------------|------|----------|---------------------------------------|
| `poll_interval_seconds` | u64  | yes      | How often to poll Horizon (seconds). Must be >= 5 and ≤ 3600. |
| `max_alerts_per_cycle` | u64 | no | Maximum webhooks sent per contract in one poll cycle. Alerts above the cap are logged (`alert cap reached: N additional alerts suppressed this cycle`) and counted in the 60-second summary, not silently dropped. Must be >= 1. Default: unlimited. A contract's own `max_alerts_per_cycle` overrides this value. |

> **Horizon rate limits:** Polling too frequently across many contracts can exhaust Horizon's per-IP request quota,
> resulting in `429 Too Many Requests` responses. Sustained polling across six or more contracts at intervals below
> 10 seconds is known to trigger rate limiting in production. The recommended minimum is
> `poll_interval_seconds = 10`; for high-volume deployments with many contracts, `poll_interval_seconds = 30` or
> higher is advised. TxWatch logs a startup warning when `poll_interval_seconds < 10`, regardless of how many
> contracts are configured. The warning includes the estimated requests per minute
> (`contracts × 60 / poll_interval_seconds`).

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
| `tags`        | table  | no       | Optional key-value pairs forwarded in every alert payload under `tags`, for routing or filtering downstream. Keys and values must be non-empty and at most 64 characters. |

> **Env var interpolation:** `webhook_secret` supports exactly one interpolation form: the whole value must be `${VAR_NAME}` (ASCII letters, digits and underscores), which is replaced by the environment variable's value at load time. Any other `$` pattern — `$VAR` without braces, `${VAR:-default}`, or a reference embedded in a longer string — is rejected at startup with an `unsupported env var interpolation` error. Values with no `$` reference are used as-is.

### Network field values

Valid `network` values and their corresponding Horizon endpoints:

| Value | Horizon URL |
|---|---|
| `mainnet` | https://horizon.stellar.org |
| `testnet` | https://horizon-testnet.stellar.org |
| `futurenet` | https://horizon-futurenet.stellar.org |

> **Futurenet explorer limitation:** explorer links for `futurenet` point to Stellar Expert (`https://stellar.expert/explorer/futurenet`). Futurenet is a development network that can be reset at any time, and Stellar Expert's coverage of it is not guaranteed. If that link does not show your transaction, use the [Stellar Laboratory](https://laboratory.stellar.org/) instead.

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

### `HighFee`
Fires when the transaction's total fee exceeds the threshold. Set exactly one of
`threshold_stroops` (raw stroops, > 0) or `threshold_xlm` (whole XLM, > 0); setting both is an error.
`threshold_xlm` is converted to stroops during validation (1 XLM = 10,000,000 stroops).

```toml
[[contracts.rules]]
type               = "HighFee"
threshold_stroops  = 100000
```

```toml
[[contracts.rules]]
type           = "HighFee"
threshold_xlm  = 1             # equivalent to threshold_stroops = 10000000
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
  "amount_stroops":   150000000000,
  "timestamp":        1705316096,
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":    "https://stellar.expert/explorer/testnet/tx/abc123...",
  "tags":             { "team": "defi", "env": "prod" }
}
```

- `function_name` — the first invoked Soroban function name (present for backward compatibility).
- `function_names` — all Soroban function names invoked in the transaction (one per `invoke_host_function` operation). Most transactions have zero or one entry.
- `tags` — the contract's `tags` table as configured, or `null` when none are set.

## Environment variables

| Variable   | Default | Description                                      |
|------------|---------|--------------------------------------------------|
| `RUST_LOG` | `info`  | Log level: `error`, `warn`, `info`, `debug`, `trace` |

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
