# Environment Variables

TxWatch reads a small set of environment variables at startup. Everything else
is configured through the TOML file described in
[configuration.md](configuration.md).

| Variable | Description | Default | Example |
| --- | --- | --- | --- |
| `TXWATCH_CONFIG` | Path to the TOML configuration file loaded at startup. | `config/example.toml` | `TXWATCH_CONFIG=/etc/txwatch/config.toml` |
| `RUST_LOG` | Log verbosity filter for the `tracing` subscriber (e.g. `info`, `debug`, or per-crate filters). | `info` | `RUST_LOG=txwatch=debug,info` |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP collector endpoint used when OpenTelemetry export is enabled. | unset (export disabled) | `OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4317` |
| `OTEL_SERVICE_NAME` | Service name reported to the OpenTelemetry collector. | `txwatch` | `OTEL_SERVICE_NAME=txwatch-prod` |
| `OTEL_SDK_DISABLED` | When `true`, disables OpenTelemetry export entirely. | `false` | `OTEL_SDK_DISABLED=true` |

## Notes

- `TXWATCH_CONFIG` is the only variable required to point TxWatch at a
  configuration file; without it the default path is used.
- `RUST_LOG` follows the standard `tracing`/`env_logger` filter syntax, so you
  can raise verbosity for a single crate without flooding logs from others.
- The `OTEL_*` variables are only consulted when OpenTelemetry export is
  compiled in and enabled; leaving them unset keeps export off.

See [configuration.md](configuration.md) for the TOML options and
[../README.md](../README.md) for a quick start.
