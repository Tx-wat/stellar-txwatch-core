# Configuration Reference

TxWatch is configured with a TOML file (see `config/example.toml` for a full
working example). This page documents the available options and how to fix the
most common configuration errors.

## Top-level options

| Key | Type | Description |
| --- | --- | --- |
| `poll_interval_seconds` | integer | How often the poller checks for new transactions. |

## `[[contracts]]`

Each `[[contracts]]` block describes one contract to watch. Blocks are
independent: `rules`, `webhook_url`, and `network` apply only to that contract.

| Key | Required | Description |
| --- | --- | --- |
| `label` | yes | Unique human-readable name for the contract. |
| `contract_id` | yes | Soroban contract address (56 characters, starts with `C`). |
| `network` | yes | One of `mainnet`, `testnet`, `futurenet`. |
| `webhook_url` | yes | Endpoint that receives alert payloads. |
| `webhook_secret` | no | HMAC-SHA256 secret sent as `X-TxWatch-Signature`. |

## `[[contracts.rules]]`

Rules are declared as an array of tables using double brackets. Each rule has a
`type` plus type-specific fields (e.g. `threshold_xlm`, `function_names`).

## Common configuration errors

### Wrong section nesting for rules

Using single brackets creates a table instead of an array element.

```toml
# Wrong
[contracts.rules]
type = "TransactionFailed"
```

Error:

```
TOML parse error: invalid type: map, expected a sequence for `contracts.rules`
```

Fix — use double brackets so each rule is an array element:

```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### Invalid network value

```toml
# Wrong
network = "main"
```

Error:

```
invalid network "main": expected one of mainnet, testnet, futurenet
```

Fix — use one of the supported networks:

```toml
network = "mainnet"
```

### Duplicate contract labels

```toml
# Wrong: two blocks share the same label
[[contracts]]
label = "Mainnet Vault"
# ...
[[contracts]]
label = "Mainnet Vault"
```

Error:

```
duplicate contract label "Mainnet Vault"
```

Fix — give every contract a unique label:

```toml
[[contracts]]
label = "Mainnet Vault"
# ...
[[contracts]]
label = "Mainnet Vault Backup"
```

### Missing required fields

```toml
# Wrong: contract_id and webhook_url are missing
[[contracts]]
label   = "No Address"
network = "testnet"
```

Error:

```
missing field `contract_id`
```

Fix — include all required fields for each contract:

```toml
[[contracts]]
label       = "My Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/my-webhook"
```
