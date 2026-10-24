# Configuration Reference

TxWatch is configured with a TOML file (see `config/example.toml` for a full
working example). This page documents the available options and how to fix the
most common configuration errors.

## Top-level options

| Key | Type | Description |
| --- | --- | --- |
| `poll_interval_seconds` | integer | How often the poller checks for new transactions. |
| `http_user_agent` | string (optional) | Custom HTTP User-Agent header for Horizon requests (default: `txwatch/<version>`). |

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
| `allow_internal_webhook_urls` | no | Per-contract opt-out of SSRF protection for local webhook development. |

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
