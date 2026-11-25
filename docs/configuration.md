# Configuration Reference

TxWatch is configured with a TOML file (see `config/example.toml` for a full
working example). This page documents the available options and how to fix the
most common configuration errors.

> Environment variables (such as `TXWATCH_CONFIG`, `RUST_LOG`, and `OTEL_*`)
> are documented separately in [environment-variables.md](environment-variables.md).

## Top-level options

| Key | Type | Description |
| --- | --- | --- |
| `poll_interval_seconds` | integer | How often the poller checks for new transactions. |
| `horizon_url` | string (optional) | Base URL of the Horizon server to poll. Defaults to the public Horizon endpoint for the network. |
| `horizon_startup_check` | boolean (optional) | When `true`, probe Horizon connectivity once at startup. The check is warn-only: failures are logged and never abort startup (default: `false`). |
| `http_user_agent` | string (optional) | Custom HTTP User-Agent header for Horizon requests (default: `txwatch/<version>`). |
| `webhook_timeout_seconds` | integer (optional) | Timeout for each webhook delivery attempt, in seconds (default: `15`). Applies to all webhook deliveries, including contracts using TLS or pinned-certificate clients. |
| `max_alerts_per_cycle` | integer (optional) | Upper bound on the number of alerts dispatched per polling cycle (default: `100`). |
| `batch_alerts_per_tx` | boolean (optional) | When `true`, alerts from multiple rules firing on the same transaction are grouped and delivered as a single webhook POST containing a `rules_triggered` array instead of one POST per rule (default: `false`). |
| `allow_internal_webhook_urls` | boolean (optional) | Global opt-out of SSRF protection for webhook URLs, allowing internal/private addresses. Can be overridden per contract (default: `false`). |
| `tls` | table (optional) | TLS settings for the webhook HTTP client. See [TLS settings](#tls-settings). |
| `webhook_tls_cert_pem` | string (optional) | Path to a PEM-encoded client certificate (or CA bundle) used for webhook TLS connections. |

### TLS settings

The optional `[tls]` table configures the webhook HTTP client:

| Key | Type | Description |
| --- | --- | --- |
| `ca_cert_pem` | string (optional) | Path to a PEM-encoded CA certificate used to verify webhook servers. |
| `client_cert_pem` | string (optional) | Path to a PEM-encoded client certificate presented to webhook servers. |
| `client_key_pem` | string (optional) | Path to the PEM-encoded private key matching `client_cert_pem`. |
| `insecure_skip_verify` | boolean (optional) | Disable certificate verification (development only; default: `false`). |

## `[[contracts]]`

Each `[[contracts]]` block describes one contract to watch. Blocks are
independent: `rules`, `webhook_url`, and `network` apply only to that contract.

> **Contract Limit (`MAX_CONTRACTS`):**
> TxWatch enforces a maximum limit of 100 contracts per configuration (`MAX_CONTRACTS = 100`).
> Each contract spawns concurrent Horizon polling routines; exceeding this limit risks high memory usage
> and socket / file-descriptor exhaustion. If you need to monitor more than 100 contracts, deploy multiple
> TxWatch instances partitioned by contract group or network.

| Key | Required | Description |
| --- | --- | --- |
| `label` | yes | Unique human-readable name for the contract. |
| `contract_id` | yes | Soroban contract address (56 characters, starts with `C`). |
| `network` | yes | One of `mainnet`, `testnet`, `futurenet`. |
| `webhook_url` | yes | Endpoint that receives alert payloads. |
| `webhook_secret` | no | HMAC-SHA256 secret sent as `X-TxWatch-Signature`. |
| `allow_internal_webhook_urls` | no | Per-contract opt-out of SSRF protection for local webhook development. Overrides the top-level setting. |
| `webhook_timeout_seconds` | no | Per-contract override of the webhook delivery timeout, in seconds. Falls back to the top-level `webhook_timeout_seconds` (default: `15`). |
| `max_alerts_per_minute` | no | Per-contract alert rate limit, in alerts per minute. When unset, no per-contract limit is applied (only the global `max_alerts_per_cycle` applies). See [Alert rate limiting](#alert-rate-limiting). |
| `alert_cooldown_secs` | no | Suppress repeated alerts with the same rule type for this many seconds. Unset disables cooldown; cooldown state is in-memory and resets when TxWatch restarts. |
| `cursor_alias` | no | Former `label` of this contract, used to migrate the persisted cursor when a contract is renamed. See [Migrating cursors for renamed contracts](#migrating-cursors-for-renamed-contracts). |
| `tags` | array of strings (optional) | Arbitrary labels attached to this contract, included in alert payloads for routing/filtering. |

### Alert rate limiting

A contract with a broad rule (for example `AnyTransaction`) on a busy contract
can produce far more alerts than a webhook receiver can handle. Set
`max_alerts_per_minute` to cap the alert rate for that contract:

```toml
[[contracts]]
label                 = "Busy Contract"
contract_id           = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network               = "mainnet"
webhook_url           = "https://hooks.example.com/busy"
max_alerts_per_minute = 30
```

Behavior:

- Each contract has its own token bucket that refills at
  `max_alerts_per_minute` tokens per minute (capacity equals the configured
  rate). Alerts consume one token each; when the bucket is empty, further
  alerts for that contract are suppressed until tokens refill.
- Suppression is per contract: a throttled contract does not affect alert
  delivery for other contracts.
- Suppressed alerts are not delivered to the webhook, but they are logged at
  `WARN` level with the contract label and the number of suppressed alerts, so
  you can detect and tune overly broad rules.
- The limit is independent of the global `max_alerts_per_cycle`; both apply.

### Alert cooldown

Set `alert_cooldown_secs` on a contract to deliver at most one alert for each
rule type during the configured interval. For example, `alert_cooldown_secs = 300`
allows one `LargeTransfer` alert and one `FunctionCalled` alert every five minutes;
one rule type does not suppress another. Cooldown timestamps are kept in memory
and reset when TxWatch restarts. Cooldown begins when delivery is attempted, even
if the webhook ultimately fails; dry-run matches do not start a cooldown.

### Migrating cursors for renamed contracts

TxWatch persists a polling cursor per contract, keyed by the contract `label`.
If you rename a contract's `label`, the cursor for the old label is no longer
found and polling restarts from "now", which can silently skip transactions
that occurred between the last poll and the restart.

To migrate the cursor when renaming a contract:

1. Note the contract's current (old) label, e.g. `Mainnet Vault`.
2. Change `label` to the new name and set `cursor_alias` to the old label:

   ```toml
   [[contracts]]
   label        = "Mainnet Vault (Primary)"
   cursor_alias = "Mainnet Vault"
   # ...other fields unchanged...
   ```

3. Restart TxWatch. On startup it looks up the cursor for `label`; if none is
   found, it falls back to the cursor stored under `cursor_alias` and continues
   polling from that position.
4. Once the contract has polled successfully under the new label, remove the
   `cursor_alias` line and restart again. The alias is only needed for the
   first start after the rename.

### Webhook Signature Verification Security Notice
When verifying the `X-TxWatch-Signature` header, receivers **must** use constant-time comparison (e.g. `hmac.compare_digest` in Python or `crypto.timingSafeEqual` in Node.js) rather than naive string comparison (`==`). Standard equality operators leak timing information through byte-by-byte short-circuiting, exposing endpoints to timing attacks.

**Python example:**
```python
import hmac, hashlib

def verify_signature(secret: str, body: bytes, signature_header: str) -> bool:
    expected = "sha256=" + hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
    return hmac.compare_digest(expected, signature_header)
```

**Node.js example:**
```javascript
const crypto = require('crypto');

function verifySignature(secret, body, signatureHeader) {
  const hmac = crypto.createHmac('sha256', secret);
  hmac.update(body);
  const expected = Buffer.from('sha256=' + hmac.digest('hex'));
  const actual = Buffer.from(signatureHeader);
  return expected.length === actual.length && crypto.timingSafeEqual(expected, actual);
}
```

## `[[contracts.rules]]`

Rules are declared as an array of tables using double brackets. Each rule has a
`type` plus type-specific fields (e.g. `threshold_xlm`, `function_names`).

The supported rule types are:

| `type` | Description |
| --- | --- |
| `TransactionFailed` | Fires when a transaction invoking the contract fails. |
| `LargeTransfer` | Fires when a transfer exceeds `threshold_xlm`. |
| `FunctionCalled` | Fires when any of the functions listed in `function_names` is invoked. |
| `SourceAccountMatch` | Fires when the transaction source account matches one of the configured accounts. |

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
