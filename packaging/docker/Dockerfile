# The build prerequisites, spelled out rather than assumed: `rustls` avoids OpenSSL, so there is
# no dylib to match against a system version, but its default `aws-lc-rs` provider is C and
# assembly and needs a C toolchain, NASM and CMake. See the README's "Build prerequisites".
FROM rust:1.88-bookworm AS build

RUN apt-get update \
    && apt-get install -y --no-install-recommends nasm cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY . .

RUN cargo build --release --locked -p volucer


# A glibc image, which is the one that matches the host distribution's libc.
FROM debian:bookworm-slim AS glibc

# `ca-certificates` is not decoration: without it an ACME client cannot verify the CA's own HTTPS
# endpoint, and the failure reads as a network error rather than as a missing trust store.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /src/target/release/volucer /usr/local/bin/volucer

RUN useradd --system --create-home --home-dir /var/lib/volucer volucer
USER volucer
WORKDIR /var/lib/volucer

EXPOSE 8080 9080
ENTRYPOINT ["/usr/local/bin/volucer"]
CMD ["serve", "--config", "/etc/volucer/volucer.toml"]


# The same binary built against musl instead of glibc: it runs on any Linux, which is what a
# scratch-based image needs and what an operator reaching for `docker run` expects from a static
# build. Two images from one crate rather than one image for each distribution.
FROM rust:1.88-alpine AS build-musl

RUN apk add --no-cache musl-dev cmake make perl bash nasm

WORKDIR /src
COPY . .

RUN cargo build --release --locked -p volucer --target x86_64-unknown-linux-musl


FROM scratch AS musl

# A static binary still verifies TLS certificates, so it still needs a trust store. There is no
# package manager in a scratch image, so the bundle is copied in and named where the binary looks
# for it (`SSL_CERT_FILE`), which is the one thing a scratch image must be told explicitly.
COPY --from=build-musl /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build-musl \
     /src/target/x86_64-unknown-linux-musl/release/volucer \
     /usr/local/bin/volucer

ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt

EXPOSE 8080 9080
ENTRYPOINT ["/usr/local/bin/volucer"]
CMD ["serve", "--config", "/etc/volucer/volucer.toml"]
