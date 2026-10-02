IMAGE ?= dvb:latest
DOCKER ?= docker
CARGO ?= cargo

.PHONY: help fmt lint test check build docker-build docker-build-cache clean all

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-18s\033[0m %s\n", $$1, $$2}'

all: check ## Format check, lint and test

fmt: ## Format the code
	$(CARGO) fmt --all

check: ## Format check + clippy (deny warnings) + tests
	$(CARGO) fmt --check
	$(CARGO) clippy --all-targets -- -D warnings
	$(CARGO) test --all-targets

lint: ## Clippy only
	$(CARGO) clippy --all-targets -- -D warnings

test: ## Run the test suite (testcontainer tests are #[ignore]d)
	$(CARGO) test --all-targets

test-all: ## Run the test suite including testcontainer-backed tests
	$(CARGO) test --all-targets -- --ignored

build: ## Release build of the dvb binary
	$(CARGO) build --release --locked --bin dvb

docker-build: ## Build the container image
	$(DOCKER) build -t $(IMAGE) .

docker-build-cache: ## Plain-cache build (deps cached in layers by cargo-chef)
	$(DOCKER) build --no-cache-filter -t $(IMAGE) .

openssl-check: ## Fail if a real TLS backend sneaks in (rustls everywhere)
	@# `openssl-probe` is a Windows-only helper inside rustls-native-certs and
	@# links nothing on Linux, so match the libraries rather than the name.
	@if $(CARGO) tree | grep -iE "openssl-sys|native-tls"; then \
		echo "ERROR: openssl/native-tls found in the dependency tree"; exit 1; \
	else \
		echo "OK: rustls only, no openssl/native-tls"; \
	fi

clean: ## Remove build artifacts
	$(CARGO) clean
