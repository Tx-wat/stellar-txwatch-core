# Builder stage: compile the txwatch binary
# TARGETARCH is provided automatically by docker buildx for multi-platform builds
FROM --platform=$BUILDPLATFORM rust:alpine AS builder
ARG TARGETARCH
WORKDIR /build
COPY . .
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconf && \
    case "$TARGETARCH" in \
        amd64) RUST_TARGET=x86_64-unknown-linux-musl ;; \
        arm64) RUST_TARGET=aarch64-unknown-linux-musl ;; \
        *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    rustup target add "$RUST_TARGET" && \
    OPENSSL_STATIC=1 cargo build --release -p txwatch --target "$RUST_TARGET" && \
    cp "target/$RUST_TARGET/release/txwatch" /build/txwatch

# Runtime stage: distroless base (CA certificates, no shell, non-root user)
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /build/txwatch /usr/local/bin/txwatch
ENTRYPOINT ["txwatch"]
