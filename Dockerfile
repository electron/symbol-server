# Rust variant of electron/ionic-kit's templates/Dockerfile.node, with the same
# runtime contract: listens on $PORT (8080), runs as uid/gid 1000 and needs no
# writable paths besides the /tmp emptyDir.

FROM rust:1.98.1-alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY rust-toolchain.toml Cargo.toml Cargo.lock ./
COPY src/ ./src/
RUN cargo build --release --locked

FROM alpine:3.22
COPY --from=build /src/target/release/symbol-server /usr/local/bin/symbol-server

# The cluster runs pods as uid/gid 1000 with a read-only root filesystem and a
# writable emptyDir at /tmp. The server writes nothing to disk; upstream TLS
# uses the bundled webpki roots, so no CA bundle is needed.
USER 1000:1000
ENV PORT=8080 \
    HOME=/tmp
EXPOSE 8080
# No HEALTHCHECK: Kubernetes probes /health (set in the prod overlay).
CMD ["symbol-server"]
