---
name: Bug Report
about: Report a reproducible bug in TxWatch
title: "[bug]: "
labels: ["bug"]
assignees: []
---

## TxWatch Version

<!-- Output of `cargo run -p txwatch -- --version` or the release tag (e.g. v0.2.0) -->

## Config Snippet

<!-- Paste a minimal config that reproduces the issue. Remove all secrets, webhook URLs, and contract IDs. -->

```toml

```

## Expected Behavior

<!-- What you expected to happen -->

## Actual Behavior

<!-- What actually happened. Include any error messages exactly as shown. -->

## Logs

<!-- Run with RUST_LOG=debug and paste the relevant output below -->

```
RUST_LOG=debug cargo run -p txwatch -- --config config/your-config.toml watch
```

## Steps to Reproduce

1.
2.
3.

## Environment

- OS:
- Rust version (`rustc --version`):
- How installed: (cargo build / Docker / pre-built binary)

## Additional Context

<!-- Any other context, screenshots, Horizon API responses, or links -->
