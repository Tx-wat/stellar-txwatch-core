# Configuration Reference

Config is a TOML file passed via `--config` (default: `config/example.toml`).

## Top-level fields

| Field                  | Type | Required | Description                          |
|------------------------|------|----------|--------------------------------------|
| `poll_interval_seconds`| u64  | yes      | How often to poll Horizon (seconds). |

## `[[contracts]]`

Each entry defines one watched Soroban contract.

| Field         | Type   | Required | Description                                      |
|---------------|--------|----------|--------------------------------------------------|
| `label`       | string | yes      | Human-readable name shown in logs and alerts.    |
| `contract_id` | string | yes      | Stellar C-address of the contract.               |
| `network`     | string | yes      | `mainnet`, `testnet`, or `futurenet`.            |
| `webhook_url` | string | yes      | HTTPS endpoint that receives `AlertPayload` JSON.|

## `[[contracts.rules]]`

At least one rule is required per contract. Rules are evaluated independently.

### `AnyTransaction`
Fires on every transaction involving the contract.
```toml
[[contracts.rules]]
type = "AnyTransaction"
```

### `TransactionFailed`
Fires when a transaction's `successful` field is `false`.
```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### `LargeTransfer`
Fires when the transferred amount meets or exceeds the threshold.
```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000
```

### `FunctionCalled`
Fires when a specific Soroban function is invoked.
```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`
Fires when any function in the list is invoked.
```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

## Webhook payload (JSON)

```json
{
  "label":            "My Escrow Contract",
  "contract_id":      "CXXX...",
  "network":          "testnet",
  "rule_triggered":   "LargeTransfer(>=10000XLM)",
  "transaction_hash": "abc123...",
  "function_name":    "transfer",
  "amount":           15000,
  "timestamp":        1705316096,
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123..."
}
```
