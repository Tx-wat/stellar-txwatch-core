# Builder stage: compile the txwatch binary
FROM rust:alpine AS builder
WORKDIR /build
COPY . .
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconf && \
    OPENSSL_STATIC=1 cargo build --release -p txwatch

# Runtime stage: distroless base (CA certificates, no shell, non-root user)
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=builder /build/target/release/txwatch /usr/local/bin/txwatch
ENTRYPOINT ["txwatch"]
