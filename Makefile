.DEFAULT_GOAL := help

.PHONY: help fmt lint test build check install install-check update e2e docs-check website-check

help: ## Show available development commands.
	@awk 'BEGIN {FS = ":.*##"}; /^[a-zA-Z0-9_-]+:.*##/ {printf "\033[36m%-14s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

fmt: ## Format Rust sources.
	cargo fmt --all

lint: ## Run Clippy with warnings denied.
	cargo clippy --all-targets --all-features -- -D warnings

test: ## Run unit and integration tests with closed stdin.
	cargo test --all-targets --no-fail-fast

build: ## Build a release binary.
	cargo build --release --locked

docs-check: ## Check internal Markdown links and Python helper syntax.
	python3 scripts/check_links.py
	python3 -m py_compile scripts/mock_openai_image_api.py scripts/check_links.py scripts/check_website.py

website-check: ## Validate the static GitHub Pages website without network access.
	python3 scripts/check_website.py

check: ## Run formatting, linting, tests, docs checks, and release build.
	cargo fmt --all -- --check
	$(MAKE) lint
	$(MAKE) test
	$(MAKE) docs-check
	$(MAKE) website-check
	$(MAKE) build
	$(MAKE) install-check

install: ## Install this local checkout into Cargo's bin directory.
	cargo install --path . --locked

install-check: ## Verify isolated install/update and PATH-based binary use.
	./scripts/verify-install.sh

update: ## Update from a specific release; set VERSION=0.1.0 (or newer).
	@test -n "$(VERSION)" || (echo "Set VERSION, e.g. make update VERSION=0.1.0"; exit 2)
	cargo install --git https://github.com/dmoliveira/codex-image-cli.git --tag v$(VERSION) --locked --force codex-image-cli

e2e: ## Run the offline tmux fake-API end-to-end certification.
	./scripts/e2e-local.sh
