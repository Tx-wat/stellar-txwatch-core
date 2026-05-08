# stellar-txwatch-core

Real-time Soroban contract monitoring and webhook alert engine for Stellar.

Part of the [Tx-wat](https://github.com/Tx-wat) ecosystem.

## Quickstart

```bash
# 1. Clone https://github.com/Tx-wat/stellar-txwatch-core
git clone 
cd stellar-txwatch-core

# 2. Copy and edit the example config
cp config/example.toml config/my-config.toml
$EDITOR config/my-config.toml

# 3. Validate
cargo run -p txwatch -- --config config/my-config.toml validate

# 4. Start monitoring
cargo run -p txwatch -- --config config/my-config.toml start
```

## CLI

```
txwatch [--config <path>] <command>

Commands:
  start            Start the polling engine
  validate         Validate the config file and exit
  list-contracts   Print all contracts defined in the config
```

`--config` defaults to `config/example.toml`.

## Config

See [docs/configuration.md](docs/configuration.md) for the full reference.

```toml
poll_interval_seconds = 10

[[contracts]]
label       = "My Escrow Contract"
contract_id = "CXXX..."
network     = "testnet"
webhook_url = "https://hooks.example.com/my-webhook"

  [[contracts.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 10000

  [[contracts.rules]]
  type           = "AdminFunctionCalled"
  function_names = ["set_admin", "upgrade", "initialize"]
```

## Alert rules

| Rule                  | Triggers when…                                      |
|-----------------------|-----------------------------------------------------|
| `AnyTransaction`      | Any transaction touches the contract                |
| `TransactionFailed`   | A transaction fails (`successful = false`)          |
| `LargeTransfer`       | Transfer amount ≥ `threshold_xlm` XLM               |
| `FunctionCalled`      | A specific Soroban function is invoked              |
| `AdminFunctionCalled` | Any function in a named list is invoked             |

See [docs/alert-rules.md](docs/alert-rules.md) for full details.

## Webhook payload

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

## Architecture

```
txwatch (cli)
  └── txwatch-poller        polls Horizon per contract, tracks cursors
        ├── txwatch-rules   evaluates AlertRules → AlertPayload
        ├── txwatch-notifier delivers payloads to webhooks (retry w/ backoff)
        └── txwatch-config  parses + validates TOML config
```

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
