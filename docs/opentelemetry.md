# OpenTelemetry Trace Export

TxWatch can export distributed traces over OTLP when built with the
`opentelemetry` feature. This guide covers enabling the feature, configuring the
exporter, and running a local tracing backend.

## Enabling the feature

Build (or install) the CLI with the `opentelemetry` feature flag:

```bash
cargo build --release --features opentelemetry
```

Without the feature the exporter is compiled out and the environment variables
below are ignored.

## Configuration

The OTLP exporter is configured entirely through standard OpenTelemetry
environment variables, so no TOML changes are required.

| Variable | Description |
| --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | Base URL of the OTLP collector/gateway that receives spans (e.g. `http://localhost:4317`). |
| `OTEL_SERVICE_NAME` | Logical service name attached to every span. Use a stable value so traces are grouped correctly in your backend. |

Example:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_SERVICE_NAME="txwatch"

txwatch --config config/example.toml
```

## Example: Jaeger

Jaeger accepts OTLP directly on its collector ports. Run it with Docker:

```bash
docker run --rm -p 16686:16686 -p 4317:4317 -p 4318:4318 \
  jaegertracing/all-in-one:latest
```

Then point TxWatch at the OTLP gRPC endpoint and open the Jaeger UI at
<http://localhost:16686>:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_SERVICE_NAME="txwatch"

txwatch --config config/example.toml
```

## Example: Grafana Tempo

Tempo ingests OTLP on port `4317` (gRPC) and `4318` (HTTP). A minimal
`docker-compose.yml`:

```yaml
services:
  tempo:
    image: grafana/tempo:latest
    command: ["-config.file=/etc/tempo.yaml"]
    volumes:
      - ./tempo.yaml:/etc/tempo.yaml
    ports:
      - "4317:4317"
      - "3200:3200"

  grafana:
    image: grafana/grafana:latest
    ports:
      - "3000:3000"
```

With `tempo.yaml` enabling the OTLP receiver:

```yaml
server:
  http_listen_port: 3200

distributor:
  receivers:
    otlp:
      protocols:
        grpc:
          endpoint: 0.0.0.0:4317

storage:
  trace:
    backend: local
    local:
      path: /tmp/tempo/traces
```

Point TxWatch at Tempo and view traces in Grafana at <http://localhost:3000>:

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT="http://localhost:4317"
export OTEL_SERVICE_NAME="txwatch"

txwatch --config config/example.toml
```

## Verifying export

If no spans appear in your backend, check that:

- the binary was built with `--features opentelemetry`;
- `OTEL_EXPORTER_OTLP_ENDPOINT` is reachable from the TxWatch host; and
- `OTEL_SERVICE_NAME` is set so spans are attributed to a service.
