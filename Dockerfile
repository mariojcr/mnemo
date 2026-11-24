# syntax=docker/dockerfile:1
# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0


# Base images pinned by the digest of the multi-arch index: a tag moves, a digest does not.
ARG BASE_REGISTRY=docker.io/library

FROM ${BASE_REGISTRY}/rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS build
ARG MNEMO_COMMIT=
ENV MNEMO_COMMIT=${MNEMO_COMMIT}
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
# compiled in: include_str! in src/update.rs
COPY deploy/release-key.pub deploy/release-key.pub
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && install -D target/release/mnemo /out/mnemo

FROM build AS test
COPY tests tests
# No ClickHouse here: the storage suite runs under `make test`, which starts one.
ENV MNEMO_TEST_CH_URL=skip
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    rustup component add rustfmt clippy >/dev/null 2>&1 \
    && cargo fmt --check && cargo clippy --locked --all-targets -q -- -D warnings && cargo test --locked -q

FROM ${BASE_REGISTRY}/rust:1.98.1-slim-trixie@sha256:4cd829461bd5c4d511c32e269da9cb8929223b666519d8004e35fc8d1d771ab7 AS release
RUN apt-get update && apt-get install -y --no-install-recommends musl-tools && rm -rf /var/lib/apt/lists/* \
    && rustup target add x86_64-unknown-linux-musl
ARG MNEMO_COMMIT=
ENV MNEMO_COMMIT=${MNEMO_COMMIT}
# First-parent commit count: `mnemo update` never goes back to a lower one unless asked to.
ARG MNEMO_SEQ=
ENV MNEMO_SEQ=${MNEMO_SEQ}
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src src
COPY web web
# compiled in: include_str! in src/update.rs
COPY deploy/release-key.pub deploy/release-key.pub
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --target x86_64-unknown-linux-musl \
    && install -D target/x86_64-unknown-linux-musl/release/mnemo /out/mnemo-linux-x86_64 \
    && cd /out && sha256sum mnemo-* > checksums.txt

FROM scratch AS release-files
COPY --from=release /out/ /

FROM ${BASE_REGISTRY}/debian:trixie-slim@sha256:a29215f6a35e51e22adffa17f89e9d2ef06214e64a2bad10d765c46aea49f11f AS runtime
# Embeddings come from an OpenAI-compatible endpoint (MNEMO_EMBED_MODEL): no model in the image.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /data mnemo
COPY LICENSE /usr/share/doc/mnemo/
COPY --from=build /out/mnemo /usr/local/bin/mnemo
USER mnemo
ENV MNEMO_HOME=/data MNEMO_LISTEN=0.0.0.0:7700
EXPOSE 7700
VOLUME /data
HEALTHCHECK --interval=10s --timeout=4s CMD ["/usr/local/bin/mnemo", "health"]
ENTRYPOINT ["/usr/local/bin/mnemo"]
# ClickHouse runs separately: MNEMO_CH_URL, MNEMO_CH_USER, MNEMO_CH_PASSWORD, MNEMO_CH_DB.
CMD ["serve"]
