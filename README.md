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

Set `RUST_LOG=debug` for verbose output. This also enables per-contract idle
poll logs — the poller emits `"no new transactions"` debug messages with the
contract label and current cursor when a poll finds nothing new.

---

## Docker

Build the image:

```bash
docker build -t txwatch .
```

Run with a config file:

```bash
docker run -v $(pwd)/config.toml:/config.toml txwatch --config /config.toml watch
```

Or validate your config:

```bash
docker run -v $(pwd)/config.toml:/config.toml txwatch --config /config.toml validate
```

For local development with webhook testing, see [Local Development with Docker Compose](#local-development-with-docker-compose) in [CONTRIBUTING.md](CONTRIBUTING.md).

---

## CLI

```
txwatch [--config <path>] <command>

Commands:
  watch [--dry-run] [--from-beginning]  Start the polling engine
  validate [--check-webhooks]           Validate the config file and print a summary
  test-webhook --url <URL>              Send a test payload to a webhook URL and exit
```

`--config` defaults to `config/example.toml`.

**watch flags:**
- `--dry-run` — evaluate rules and log matches, but do not send any webhooks
- `--from-beginning` — start polling from the earliest available transaction (cursor `0`) instead of `now`. Overrides any persisted `cursor_file` values for this run (does not modify the file). Warning: may generate many alerts for historical transactions.

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
| `LargeTransfer` | Payment amount ≥ `threshold_xlm` XLM, or ≥ `threshold_stroops` stroops (exactly one) |
| `FunctionCalled` | A specific Soroban function is invoked |
| `AdminFunctionCalled` | Any function in a named list is invoked |
| `HighFee` | Transaction fee exceeds configured threshold |

See [docs/alert-rules.md](docs/alert-rules.md) for full details.

---

## Webhook payload

```json
{
  "label":            "My Escrow Contract",
  "contract_id":      "CAAA...",
  "network":          "testnet",
  "rule_type":        "LargeTransfer",
  "rule_triggered":   "LargeTransfer(>=10000XLM)",
  "transaction_hash": "abc123...",
  "function_name":    "transfer",
  "function_names":   ["transfer"],
  "amount_xlm":       15000,
  "amount_stroops":   150000000000,
  "fee_charged_stroops": 50000,
  "memo_type":        "text",
  "memo":             "order-1234",
  "timestamp":        1705316096,
  "timestamp_iso":    "2024-01-15T12:00:00Z",
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":    "https://stellar.expert/explorer/testnet/tx/abc123...",
  "ledger_sequence":  12345678,
  "source_account":   "GABC..."
}
```

**Webhook headers:**
- `Content-Type: application/json`
- `Content-Length: <length of JSON body in bytes>`
- `X-TxWatch-Version: <package version>`
- `X-TxWatch-Signature: sha256=<hmac>` (optional, only when `webhook_secret` is configured — HMAC-SHA256 of the request body)

**Fields:**
- `rule_type` — stable machine-readable rule variant (e.g. `"LargeTransfer"`, `"HighFee"`); use this for programmatic routing
- `rule_triggered` — human-readable rule description with parameters (e.g. `"LargeTransfer(>=10000XLM)"`); use this for display
- `function_name` — the first invoked Soroban function name, or `null` for non-Soroban transactions.
- `function_names` — all invoked Soroban function names in the transaction (may contain multiple entries for multi-op transactions).
- `memo_type` — the transaction memo type from Horizon (`"text"`, `"hash"`, `"id"`, `"return"` or `"none"`), or `null` if Horizon did not return one.
- `memo` — the transaction memo value (e.g. an order ID or payment reference), or `null` when the transaction has no memo.
- `horizon_link` — direct Horizon REST API URL for the transaction (e.g. `https://horizon-testnet.stellar.org/transactions/<hash>`); useful for fetching raw XDR or operation details programmatically.
- `explorer_link` — Stellar Expert web explorer URL for the transaction (e.g. `https://stellar.expert/explorer/testnet/tx/<hash>`); useful for human-readable inspection in a browser.
- `ledger_sequence` — the Stellar ledger (block) sequence number this transaction was included in; always present when Horizon returns it, otherwise `null`. Use this to cross-reference with Stellar ledger data or detect gaps in monitored activity.
- `source_account` — the Stellar account that signed and submitted the transaction (e.g. `"GABC..."`); always present when Horizon returns it, otherwise `null`. Useful for knowing which user or bot triggered the alert and for filtering alerts by caller in downstream systems.

> **`horizon_link` vs `explorer_link`**: `horizon_link` points to the Horizon REST API endpoint and returns raw JSON — useful for programmatic access. `explorer_link` points to the [Stellar Expert](https://stellar.expert) web UI — useful for human inspection. Both are always present for every alert regardless of rule type.

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

## Tracing and observability

TxWatch uses `tracing` spans to correlate work across each poll cycle and webhook delivery.
- `txwatch-poller::poll_contract` is instrumented with `contract`, `contract_id`, and `network` fields.
- `txwatch-poller::fetch_soroban_details` is instrumented with the transaction hash.
- `txwatch-notifier::send_webhook` creates a span with `contract` and `rule` fields before sending the webhook request.

Set `RUST_LOG=info` or a more specific filter to view structured tracing output in the CLI.

### Prometheus metrics (optional)

Build with the `metrics` feature to expose Prometheus counters and an HTTP `/metrics` endpoint:

```bash
cargo build -p txwatch-poller --features metrics
```

Three counters are registered:

| Counter | Description |
|---|---|
| `txwatch_transactions_total` | Total Stellar transactions processed across all watched contracts |
| `txwatch_alerts_total` | Total alert payloads sent (rules matched) |
| `txwatch_webhook_failures_total` | Total permanent webhook delivery failures (after all retries) |

To start the `/metrics` endpoint, call `txwatch_poller::serve_metrics(addr)` before `run_with`.
The endpoint serves the standard Prometheus text exposition format and can be scraped by any
Prometheus-compatible monitoring stack (Prometheus, Grafana Agent, VictoriaMetrics, etc.).

Example scrape config:

```yaml
scrape_configs:
  - job_name: txwatch
    static_configs:
      - targets: ['localhost:9090']
```

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
