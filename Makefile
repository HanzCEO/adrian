# Convenience targets. Everything here is also runnable directly.

COMPOSE := docker compose -f docker/docker-compose.yml
CARGO := cargo
EMBEDDER_DIR := embedder

.DEFAULT_GOAL := help

.PHONY: help
help:
	@printf 'Adrian v0.1.0\n\n'
	@printf '  make build     build both cargo workspaces\n'
	@printf '  make test      run both test suites\n'
	@printf '  make check     fmt, clippy, and tests\n'
	@printf '  make fmt       format both workspaces\n'
	@printf '  make clippy    lint both workspaces\n'
	@printf '  make up        build and start the compose stack\n'
	@printf '  make down      stop the stack and remove volumes\n'
	@printf '  make logs      follow the stack logs\n'
	@printf '  make weights   fetch and verify the model weights\n'
	@printf '  make golden    regenerate the reference embeddings\n'
	@printf '  make smoke     end-to-end test against the compose stack\n'

.PHONY: build
build:
	$(CARGO) build --workspace
	cd $(EMBEDDER_DIR) && $(CARGO) build

.PHONY: test
test:
	$(CARGO) test --workspace
	cd $(EMBEDDER_DIR) && $(CARGO) test

.PHONY: fmt
fmt:
	$(CARGO) fmt --all
	cd $(EMBEDDER_DIR) && $(CARGO) fmt --all

.PHONY: clippy
clippy:
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	cd $(EMBEDDER_DIR) && $(CARGO) clippy --all-targets -- -D warnings

.PHONY: check
check: fmt clippy test

.PHONY: up
up:
	$(COMPOSE) up -d --build

.PHONY: down
down:
	$(COMPOSE) down -v

.PHONY: logs
logs:
	$(COMPOSE) logs -f

.PHONY: weights
weights:
	scripts/fetch_weights.sh

.PHONY: golden
golden:
	cd scripts/oracle && uv run python gen_golden.py

.PHONY: smoke
smoke:
	scripts/smoke.sh
