# stellar-txwatch-core

> Real-time Soroban smart contract monitoring and webhook alert engine for the Stellar network.

Part of the [TxWatch](https://github.com/Tx-wat) ecosystem.

---

## What is this?

**TxWatch** sits between the [Stellar Horizon REST API](https://developers.stellar.org/api/horizon)
and your infrastructure. It polls every contract you configure, evaluates alert rules against
each new transaction, and fires a JSON webhook the moment a condition is met — no SDK, no
subscriptions, no infrastructure beyond a single Rust binary.

```
  Stellar Network
       │
       ▼
  Horizon REST API          ← TxWatch polls this
  (testnet / mainnet /
   futurenet)
       │
       ▼
  txwatch-poller            ← fetches /accounts/{contract}/transactions
       │                       fetches /transactions/{hash}/operations
       ▼
  txwatch-rules             ← evaluates AlertRules against each transaction
       │
       ▼
  txwatch-notifier          ← POSTs AlertPayload JSON to your webhook URL
       │
       ▼
  Your webhook receiver     ← Slack, PagerDuty, custom API, etc.
```

---

## Stellar / Soroban primer

| Concept | What it means here |
|---|---|
| **Stellar** | Layer-1 blockchain with fast finality (~5 s) and low fees |
| **Soroban** | Stellar's smart contract platform (WebAssembly-based) |
| **Horizon** | The REST API gateway to the Stellar network — TxWatch's data source |
| **Contract address** | A 56-character string starting with `C` (e.g. `CABC...`) |
| **XLM** | Stellar's native asset; 1 XLM = 10,000,000 stroops |
| **Stroop** | Smallest unit of XLM (like satoshi for Bitcoin) |
| **Paging token** | Horizon cursor used to fetch only new transactions since last poll |
| **invoke_host_function** | The Horizon operation type for a Soroban contract call |

### Horizon endpoints used

| Endpoint | Purpose |
|---|---|
| `GET /accounts/{contract_id}/transactions?cursor=…&order=asc` | Fetch new transactions for a contract |
| `GET /transactions/{hash}/operations` | Fetch operations to extract function name and payment amount |

### Network base URLs

| Network | Horizon base URL |
|---|---|
| Mainnet | `https://horizon.stellar.org` |
| Testnet | `https://horizon-testnet.stellar.org` |
| Futurenet | `https://horizon-futurenet.stellar.org` |

---

## Quickstart

```bash
# 1. Clone
git clone https://github.com/Veritas-Vaults-Network/stellar-txwatch-core
cd stellar-txwatch-core

# 2. Copy and edit the example config
cp config/example.toml config/my-config.toml
$EDITOR config/my-config.toml

# 3. Validate your config
cargo run -p txwatch -- --config config/my-config.toml validate

# 4. Send a test webhook to confirm your receiver works
cargo run -p txwatch -- --config config/my-config.toml \
  test-webhook --url https://hooks.example.com/my-webhook

# 5. Start watching
cargo run -p txwatch -- --config config/my-config.toml watch
```

Set `RUST_LOG=debug` for verbose output.

---

## CLI

```
txwatch [--config <path>] <command>

Commands:
  watch                        Start the polling engine
  validate                     Validate the config file and print a summary
  test-webhook --url <URL>     Send a test payload to a webhook URL and exit
```

`--config` defaults to `config/example.toml`.

---

## Config

See [docs/configuration.md](docs/configuration.md) for the full reference.

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

---

## Alert rules

| Rule | Triggers when… |
|---|---|
| `AnyTransaction` | Any transaction touches the contract |
| `TransactionFailed` | A transaction fails (`successful = false`) |
| `LargeTransfer` | Payment amount ≥ `threshold_xlm` XLM |
| `FunctionCalled` | A specific Soroban function is invoked |
| `AdminFunctionCalled` | Any function in a named list is invoked |

See [docs/alert-rules.md](docs/alert-rules.md) for full details.

---

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

`horizon_link` is a direct URL to the transaction on Horizon — paste it into a browser
or the [Stellar Expert explorer](https://stellar.expert) to inspect the full XDR.

---

## Architecture

```
txwatch (cli binary)
  │
  ├── txwatch-config      TOML parsing · contract ID validation · rule validation
  │
  └── txwatch-poller      Horizon polling loop · cursor tracking · op enrichment
        │
        ├── txwatch-rules     AlertRule evaluation · AlertPayload construction
        │
        └── txwatch-notifier  Webhook POST · 3-attempt exponential backoff · tracing logs
```

### Crate responsibilities

| Crate | Responsibility |
|---|---|
| `txwatch-config` | Parse `config.toml` into typed structs; validate all fields |
| `txwatch-rules` | Pure rule evaluation — no I/O, fully unit-testable |
| `txwatch-notifier` | HTTP webhook delivery with retry; timestamped structured logs |
| `txwatch-poller` | Horizon REST client; cursor map; per-transaction error isolation |
| `txwatch` (cli) | `clap` binary; tracing init; subcommand dispatch |

---

## How a transaction flows through TxWatch

```
1. Poller wakes up (every poll_interval_seconds)
2. For each contract:
   a. GET /accounts/{contract_id}/transactions?cursor={last_seen}&order=asc
   b. For each new transaction:
      i.  Advance cursor (even if enrichment fails)
      ii. GET /transactions/{hash}/operations
          → extract function_name from invoke_host_function ops
          → extract amount_stroops from payment ops
      iii. Build EnrichedTransaction
      iv.  Evaluate all AlertRules → Vec<AlertPayload>
      v.   POST each AlertPayload to webhook_url (retry up to 3×)
```

---

## Stellar testnet resources

| Resource | URL |
|---|---|
| Testnet Horizon | https://horizon-testnet.stellar.org |
| Stellar Expert (testnet) | https://stellar.expert/explorer/testnet |
| Stellar Laboratory | https://laboratory.stellar.org |
| Friendbot (fund testnet accounts) | https://friendbot.stellar.org |
| Soroban docs | https://developers.stellar.org/docs/smart-contracts |

---

## Sister repos

| Repo | Description |
|---|---|
| [stellar-txwatch-web](https://github.com/Veritas-Vaults-Network/stellar-txwatch-web) | Web dashboard for alert history and contract management |
| [stellar-txwatch-contracts](https://github.com/Veritas-Vaults-Network/stellar-txwatch-contracts) | Example Soroban contracts to monitor with TxWatch |

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
