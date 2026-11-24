# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- JSON Schema export with `txwatch schema` and a CI check for configuration schema drift (#522)
- Four additional alert rule types: `InsufficientFunds`, `SorobanInvocation`, `SourceAccountMatch`, `TimeWindow`
- mTLS support for webhook delivery, including pinned-certificate verification
- SSRF guard for webhook URLs, rejecting requests to private/internal addresses
- Webhook idempotency keys to enable deduplication of alert deliveries
- Per-contract tags for organizing and filtering monitored contracts
- Cursor persistence across restarts to resume polling without gaps or duplicates
- Prometheus metrics endpoint for monitoring polling, alerting, and webhook delivery
- Graceful shutdown handling for in-flight polls and webhook deliveries
- Configurable webhook request timeout
- Escalation on cursor-save failures to surface persistence errors

### Changed

- Webhook delivery now applies the SSRF guard and mTLS/pinned-cert settings to all outbound requests
- Cursor state is persisted so polling resumes from the last processed ledger on restart

### Fixed

- Cursor-save failures are now escalated rather than silently ignored, preventing unnoticed polling gaps

## [0.1.0] - 2025-01-01

### Added

- Real-time Soroban smart contract monitoring and webhook alert engine
- Six alert rule types: `AnyTransaction`, `TransactionFailed`, `LargeTransfer`, `FunctionCalled`, `AdminFunctionCalled`, `HighFee`
- Horizon REST API integration with cursor-based pagination for efficient polling
- TOML-based configuration with contract, rule, and webhook setup
- Support for multiple Stellar networks: mainnet, testnet, futurenet
- Webhook notification delivery with exponential backoff retry logic (up to 3 attempts)
- Webhook secret signing via `X-TxWatch-Secret` header
- Structured JSON alert payloads with transaction details and Horizon links
- Transaction enrichment with operation-level details (function names, transfer amounts)
- Fee extraction and analysis for cost monitoring
- Configurable polling intervals
- CLI commands: `watch`, `validate`, `test-webhook`
- Comprehensive configuration reference and alert rules documentation
- Integration test suite using wiremock for HTTP mocking

---

[Keep a Changelog]: https://keepachangelog.com/en/1.0.0/
