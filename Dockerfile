# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1-trixie AS builder
ARG TARGETARCH
ARG CARGO_BUILD_JOBS=2

RUN apt-get update && apt-get install -y --no-install-recommends \
    gcc-aarch64-linux-gnu g++-aarch64-linux-gnu libc6-dev-arm64-cross && \
    rm -rf /var/lib/apt/lists/* && \
    rustup target add aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu

ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=liteparse-target-${TARGETARCH},target=/cargo-target \
    case "$TARGETARCH" in \
        amd64) rust_target=x86_64-unknown-linux-gnu ;; \
        arm64) rust_target=aarch64-unknown-linux-gnu; \
            export CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
                CXX_aarch64_unknown_linux_gnu=aarch64-linux-gnu-g++ ;; \
        *) echo "Unsupported architecture: $TARGETARCH" >&2; exit 1 ;; \
    esac && \
    CARGO_TARGET_DIR=/cargo-target cargo build --release --locked --target "$rust_target" \
        -p liteparse --bin lit --no-default-features --features oar-ocr && \
    install -D "/cargo-target/$rust_target/release/lit" /out/lit && \
    install -D "/cargo-target/$rust_target/release/deps/libpdfium.so" /out/libpdfium.so

FROM debian:trixie-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates libgcc-s1 libstdc++6 bash jq && \
    rm -rf /var/lib/apt/lists/*

COPY --from=builder /out/lit /usr/local/bin/lit
COPY --from=builder /out/libpdfium.so /usr/local/lib/libpdfium.so
COPY crates/liteparse/licenses/Go-JPEG-LICENSE /usr/share/licenses/liteparse/Go-JPEG-LICENSE
COPY LICENSE /usr/share/licenses/liteparse/LICENSE
COPY --chmod=755 scripts/pdf-tool /usr/local/bin/pdf-tool
ENV PDFIUM_LIB_PATH=/usr/local/lib

RUN ln -s /usr/local/bin/lit /usr/local/bin/liteparse

CMD ["lit", "--help"]
