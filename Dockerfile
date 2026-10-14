# syntax=docker/dockerfile:1

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
# libstdc++ for ONNX Runtime. The model directory is created 755 here: BuildKit's ADD gives
# missing parents its --chmod mode (644, not traversable).
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libstdc++6 \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --create-home --home-dir /data mnemo \
    && install -d -m 755 /usr/local/share/mnemo/embeddinggemma-2/onnx
ADD --checksum=sha256:db14e4863bd37893fc59729d986ab2a0d043d10b7d44da1913c4982b7e3d009c \
    https://github.com/microsoft/onnxruntime/releases/download/v1.28.3/onnxruntime-linux-x64-1.28.3.tgz /tmp/ort.tgz
RUN tar -xzf /tmp/ort.tgz -C /tmp \
    && cp /tmp/onnxruntime-linux-x64-1.28.3/lib/libonnxruntime.so.1.28.3 /usr/local/lib/ \
    && ln -s libonnxruntime.so.1.28.3 /usr/local/lib/libonnxruntime.so \
    && rm -rf /tmp/ort.tgz /tmp/onnxruntime-linux-x64-1.28.3
ENV ORT_DYLIB_PATH=/usr/local/lib/libonnxruntime.so
# Before the binary: the model layer does not change between builds, so it is reused.
ARG EG2=https://huggingface.co/onnx-community/embeddinggemma-2-ONNX/resolve/daa72c51243991dfcaf9f9137d2c573d8f7790c0
ADD --chmod=644 --checksum=sha256:bc47de15f81208a5c99e5ab10f746d5e33b51ea228b7dc0bef9c133a94f1c1c3 \
    ${EG2}/onnx/model.onnx /usr/local/share/mnemo/embeddinggemma-2/onnx/model.onnx
ADD --chmod=644 --checksum=sha256:9fd452bfc6916e5a92f080f8dada35598ca4512b21c65897d4c5974d5f9630a2 \
    ${EG2}/onnx/model.onnx_data /usr/local/share/mnemo/embeddinggemma-2/onnx/model.onnx_data
ADD --chmod=644 --checksum=sha256:4d777ef5bdc1aa36227abdfb77c3e49e7b9c892d16e1b6bda41c393504828be4 \
    ${EG2}/tokenizer.json /usr/local/share/mnemo/embeddinggemma-2/tokenizer.json
ENV MNEMO_EMBED_DIR=/usr/local/share/mnemo/embeddinggemma-2
COPY LICENSE /usr/share/doc/mnemo/
COPY --from=build /out/mnemo /usr/local/bin/mnemo
USER mnemo
# If the image user cannot read the library or the model, mnemo still starts, the embedder never
# answers and distillation waits forever: better to fail the build.
RUN for f in "$ORT_DYLIB_PATH" "$MNEMO_EMBED_DIR/onnx/model.onnx" "$MNEMO_EMBED_DIR/onnx/model.onnx_data" \
        "$MNEMO_EMBED_DIR/tokenizer.json"; do \
      test -r "$f" || { echo "the mnemo user cannot read $f" >&2; exit 1; }; \
    done
ENV MNEMO_HOME=/data MNEMO_LISTEN=0.0.0.0:7700
EXPOSE 7700
VOLUME /data
HEALTHCHECK --interval=10s --timeout=4s CMD ["/usr/local/bin/mnemo", "health"]
ENTRYPOINT ["/usr/local/bin/mnemo"]
# ClickHouse runs separately: MNEMO_CH_URL, MNEMO_CH_USER, MNEMO_CH_PASSWORD, MNEMO_CH_DB.
CMD ["serve"]
