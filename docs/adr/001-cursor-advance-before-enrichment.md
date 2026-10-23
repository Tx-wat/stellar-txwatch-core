# ADR 001: Advance Cursor Before Transaction Enrichment

## Status
Accepted

## Context
When TxWatch polls Horizon for contract transactions, each retrieved transaction must undergo operation enrichment (`EnrichedTransaction::from_horizon` and Soroban operation detail extraction) to inspect function calls, parameters, and invocation arguments.

In rare scenarios, Horizon data for a specific transaction may be malformed, corrupted, or incompatible with the enrichment parser. If the poller were to fail the poll cycle or abort without updating the contract's paging cursor, the next poll cycle would re-request the exact same failed transaction from Horizon. This would cause an infinite retry loop, stalling ingestion for all subsequent valid transactions on that contract.

## Options Considered

1. **Advance Cursor Only After Successful Enrichment and Notification Delivery**:
   - *Pros*: Ensures at-least-once delivery; guarantees alerts are fired before the cursor moves past a transaction.
   - *Cons*: Poison-pill transactions completely stall the contract pipeline. Manual operator intervention (editing persisted cursor files) would be required to unblock polling.

2. **Retry with Exponential Backoff and Poison-Pill DLQ (Dead-Letter Queue)**:
   - *Pros*: Provides visibility into skipped transactions.
   - *Cons*: Introduces significant state management overhead, external storage requirements, and complexity into a lightweight poller.

3. **Advance Cursor Before Enrichment and Increment an Error Metric**:
   - *Pros*: Poison-pill transactions are safely bypassed, allowing the poller to continue processing subsequent contract transactions without operator intervention. Data quality failures are visible via warnings in logs and the `txwatch_enrichment_errors_total` Prometheus metric.
   - *Cons*: An unparseable transaction will not trigger alert evaluation.

## Decision
We chose Option 3: advance the cursor before performing operation enrichment. When a transaction is encountered in the batch from Horizon, its paging token is immediately committed to the in-memory cursor map. If enrichment encounters an error, a warning is logged, the `txwatch_enrichment_errors_total` metric is incremented, and processing moves on to the next transaction.

## Consequences
- **Positive**: Polling loops are resilient and self-healing against malformed or unexpected Horizon payloads.
- **Positive**: Stalled ingestion pipelines on contracts are prevented.
- **Negative / Trade-off**: Malformed transactions cannot be evaluated for alerts. Monitoring systems must scrape `txwatch_enrichment_errors_total` to detect recurrent data quality issues.
