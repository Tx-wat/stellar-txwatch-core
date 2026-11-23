# TxWatch operations runbook

## Expired Horizon cursor (410 Gone)

Look for the contract label and the expired cursor in the warning log. TxWatch
resets the cursor to `now` and resumes on the next poll. Events between the old
cursor and the new position can be missed. Record the affected contract and
time range, then reconcile against a Horizon endpoint that still retains the
history or an archival source. If replaying, use a separate cursor file and a
test webhook receiver first to avoid duplicate production alerts.

## Webhook delivery failures

Check the delivery log for the HTTP status, contract and transaction hash.
Verify DNS, TLS trust, receiver health, timeouts and credentials. A 401 or 403
usually calls for checking the receiver's credentials or signature validation.
TxWatch treats 2xx as success, does not follow redirects, and makes at most
three attempts with backoff for retryable failures. Failed webhook attempts do
not rewind the Horizon cursor. Reconcile missed alerts separately, using the
idempotency key to deduplicate any replay.

## Horizon rate limiting (429)

Check the contract and backoff duration in the warning log. TxWatch waits for
the numeric `Retry-After` value, or five seconds if it is absent or invalid.
If rate limits persist, increase `poll_interval_seconds`, reduce shared
traffic, or switch to an endpoint with a larger quota. Check that each contract
is catching up after the change.

## Config reload

1. Edit a copy of the active TOML and run `txwatch --config /path/to/copy.toml validate`.
   Run it with the same environment variables as the service.
2. Replace the active file atomically in the same directory. TxWatch watches
   the directory and reloads after the change.
3. Look for `config reloaded` or inspect `txwatch_config_reload_total`. If the
   reload fails, TxWatch logs the error and keeps the previous config.
4. Fix the candidate file and repeat. If the file watcher is disabled, restart
   the service with the validated config and its existing cursor file.

A process environment change needs a restart. Restrict permissions on config
files that contain webhook credentials and avoid including header values in
incident reports.

To supply an API key, add a header map to the relevant contract block:

```toml
webhook_headers = { "X-Api-Key" = "${RECEIVER_API_KEY}" }
```

TxWatch reserves its signing, idempotency and transport headers. Header names
are case-insensitive. For a rule that fires when a function occurs at least
three times in one transaction, configure
`{ type = "FunctionCallCount", function_name = "transfer", min_count = 3 }`.
Function names match case exactly.
