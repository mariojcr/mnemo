# SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
# SPDX-License-Identifier: Apache-2.0

.PHONY: build test test-ch test-replicated test-replicated-ch fmt lint integration image bench run release

RUST_IMAGE ?= docker.io/library/rust:1.98.1-slim-trixie
IMAGE ?= mnemo:local
COMMIT ?= $(shell git rev-parse --short=7 HEAD 2>/dev/null)
SEQ ?= $(shell git rev-list --first-parent --count HEAD 2>/dev/null)
CH_IMAGE ?= clickhouse/clickhouse-server:26.9
CARGO_RUN = docker run --rm --network host -e CARGO_HOME=/cargo -e RUST_LOG -e MNEMO_TEST_URL -e MNEMO_TEST_KEY \
	-e MNEMO_TEST_CH_USER -e MNEMO_TEST_CH_PASSWORD \
	-v mnemo-cargo:/cargo -v "$(CURDIR):/src" -w /src
CARGO = $(CARGO_RUN) -e MNEMO_TEST_CH_URL=http://127.0.0.1:18123 $(RUST_IMAGE)

build:
	$(CARGO) cargo build --release --locked

# One ephemeral ClickHouse for the whole suite: each test creates and drops its own database.
TEST_CH = mnemo-test-clickhouse
test-ch:
	@if ! docker inspect $(TEST_CH) >/dev/null 2>&1; then \
		CLICKHOUSE_PASSWORD=$$(openssl rand -hex 24) || exit 1; export CLICKHOUSE_PASSWORD; \
		docker run -d --rm --name $(TEST_CH) -p 127.0.0.1:18123:8123 \
			--ulimit nofile=262144:262144 -e CLICKHOUSE_USER=mnemo -e CLICKHOUSE_PASSWORD \
			-e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=0 $(CH_IMAGE) >/dev/null; fi
	@for i in $$(seq 60); do curl -fs 127.0.0.1:18123/ping >/dev/null && exit 0; sleep 1; done; echo "test ClickHouse is not answering"; exit 1

# Read the credential from the fixture when reusing it; keep it out of command output.
CH_PASSWORD = docker inspect --format '{{range .Config.Env}}{{println .}}{{end}}'

test: test-ch
	@MNEMO_TEST_CH_PASSWORD=$$($(CH_PASSWORD) $(TEST_CH) | sed -n 's/^CLICKHOUSE_PASSWORD=//p'); \
		test -n "$$MNEMO_TEST_CH_PASSWORD" || { echo "test ClickHouse has no password"; exit 1; }; \
		export MNEMO_TEST_CH_PASSWORD; \
		$(CARGO) sh -c 'rustup component add rustfmt clippy >/dev/null 2>&1; cargo fmt --check && cargo clippy --locked --all-targets -q -- -D warnings && cargo test --locked -q'

# Not part of `make test`: slow. The `#[ignore]` tests in storage::ch need the replicas.
# To remove them: docker rm -f mnemo-test-ch1 mnemo-test-ch2 mnemo-test-keeper
TEST_RCH_NET = mnemo-test-replicated
test-replicated-ch:
	@docker network inspect $(TEST_RCH_NET) >/dev/null 2>&1 || docker network create $(TEST_RCH_NET) >/dev/null
	@docker inspect mnemo-test-keeper >/dev/null 2>&1 || docker run -d --rm --name mnemo-test-keeper --network $(TEST_RCH_NET) \
		-v "$(CURDIR)/tests/replicated/keeper.xml:/etc/keeper.xml:ro" --entrypoint /usr/bin/clickhouse $(CH_IMAGE) \
		keeper --config-file /etc/keeper.xml >/dev/null
	@CLICKHOUSE_PASSWORD=; \
		for i in 1 2; do \
			if docker inspect mnemo-test-ch$$i >/dev/null 2>&1; then \
				CLICKHOUSE_PASSWORD=$$($(CH_PASSWORD) mnemo-test-ch$$i | sed -n 's/^CLICKHOUSE_PASSWORD=//p'); \
				test -n "$$CLICKHOUSE_PASSWORD" || { echo "test replica has no password"; exit 1; }; \
				break; fi; done; \
		if [ -z "$$CLICKHOUSE_PASSWORD" ]; then CLICKHOUSE_PASSWORD=$$(openssl rand -hex 24) || exit 1; fi; \
		export CLICKHOUSE_PASSWORD; \
		for i in 1 2; do docker inspect mnemo-test-ch$$i >/dev/null 2>&1 || docker run -d --rm --name mnemo-test-ch$$i \
		--hostname mnemo-test-ch$$i --network $(TEST_RCH_NET) -p 127.0.0.1:1812$$((3 + i)):8123 --ulimit nofile=262144:262144 \
		-e CH_REPLICA=mnemo-test-ch$$i -e CLICKHOUSE_USER=mnemo -e CLICKHOUSE_PASSWORD -e CLICKHOUSE_DEFAULT_ACCESS_MANAGEMENT=0 \
		-v "$(CURDIR)/tests/replicated/replica.xml:/etc/clickhouse-server/config.d/replica.xml:ro" $(CH_IMAGE) >/dev/null; done
	@for i in $$(seq 60); do curl -fs 127.0.0.1:18124/ping >/dev/null && curl -fs 127.0.0.1:18125/ping >/dev/null && exit 0; \
		sleep 1; done; echo "test replicas are not answering"; exit 1

test-replicated: test-replicated-ch
	@MNEMO_TEST_CH_PASSWORD=$$($(CH_PASSWORD) mnemo-test-ch1 | sed -n 's/^CLICKHOUSE_PASSWORD=//p'); \
		test -n "$$MNEMO_TEST_CH_PASSWORD" || { echo "test replica has no password"; exit 1; }; \
		export MNEMO_TEST_CH_PASSWORD; \
		$(CARGO_RUN) -e MNEMO_TEST_CH_URL=http://127.0.0.1:18124 -e MNEMO_TEST_CH_REPLICAS=http://127.0.0.1:18124,http://127.0.0.1:18125 \
		$(RUST_IMAGE) cargo test --locked -q storage:: -- --include-ignored

fmt:
	$(CARGO) sh -c 'rustup component add rustfmt >/dev/null 2>&1; cargo fmt'

# Against a running server: MNEMO_TEST_URL=http://127.0.0.1:7700 [MNEMO_TEST_KEY=…] make integration
integration:
	$(CARGO) cargo test --locked -q --features integration --test service

image:
	docker build --target runtime --build-arg MNEMO_COMMIT=$(COMMIT) -t $(IMAGE) .

bench:
	$(CARGO) cargo run --release --locked -q -- bench $(or $(N),1000000)

# Needs a ClickHouse at MNEMO_CH_URL (plus MNEMO_CH_USER, MNEMO_CH_PASSWORD, MNEMO_CH_DB) and a
# way to sign in: MNEMO_ADMIN_KEY, a key (`mnemo key create`) or OIDC.
run: build
	MNEMO_HOME=$(CURDIR)/.mnemo ./target/release/mnemo serve

release:
	docker build --target release-files --build-arg MNEMO_COMMIT=$(COMMIT) --build-arg MNEMO_SEQ=$(SEQ) -o type=local,dest=dist .
