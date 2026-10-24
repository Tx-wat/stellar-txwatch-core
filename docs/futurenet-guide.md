# Running TxWatch on Futurenet

Futurenet is the Stellar test network dedicated to testing Soroban smart contract features and previewing protocol upgrades. This guide explains how to monitor Futurenet contracts with TxWatch.

## 1. Network Configuration

TxWatch has built-in support for Futurenet. Set `network = "futurenet"` on your watched contracts:

- Horizon base URL: `https://horizon-futurenet.stellar.org`
- Stellar Expert explorer: `https://stellar.expert/explorer/futurenet`

### Example `config.toml`

```toml
poll_interval_seconds = 10

[[contracts]]
label = "Futurenet Token Contract"
contract_id = "CDLZFC3SYJYDZT7K67VZ75HPJVIEUVNIXF47ZG2FB2RMQQVU2HHGCYSC"
network = "futurenet"
webhook_url = "https://hooks.example.com/futurenet"

[[contracts.rules]]
type = "AnyTransaction"

[[contracts.rules]]
type = "LargeTransfer"
threshold_xlm = 100
```

## 2. Funding Accounts with Friendbot

To submit test transactions or deploy contracts on Futurenet, accounts can be funded using the Futurenet Friendbot:

```bash
curl "https://friendbot-futurenet.stellar.org/?addr=<YOUR_PUBLIC_KEY>"
```

Friendbot credits the target account with test lumens (XLM) immediately.

## 3. Gotchas & Known Limitations

- **Periodic Network Resets**: Futurenet is periodically reset (usually during protocol version upgrades). When a reset occurs:
  - All ledger history, accounts, and deployed contracts are wiped.
  - Previously saved cursors in `cursor_file` will reference non-existent ledger sequences or paging tokens.
  - **Recovery**: Delete the cursor file (or start with `--from-beginning` / fresh cursor) after each network reset.
- **Contract Redeployment**: After a reset, contracts must be redeployed to new contract IDs, requiring an update to your `config.toml`.
- **Latency & Availability**: Futurenet nodes may have variable availability during protocol upgrade testing.
