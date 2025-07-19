# Makefile for local development of the Engineering Vlog powered by Zola
# Usage:
#   make install   – install zola (via cargo) if it is missing
#   make serve     – start live-reload dev server on http://localhost:1111
#   make build     – generate static site into ./public
#   make clean     – remove generated artifacts
#
# Keep this file <300 lines for repo rule compliance.

ZOLA_VERSION := 0.19.1

# Default to cargo-installed path if not in PATH
ifeq ($(ZOLA),)
ZOLA := $(HOME)/.cargo/bin/zola
endif

# Detect package managers
BREW := $(shell command -v brew 2>/dev/null)
CARGO := $(shell command -v cargo 2>/dev/null)

# Default target
.DEFAULT_GOAL := help

.PHONY: help
help:
	@echo "Available targets:"
	@echo "  install  – install the Zola CLI if it is not already present"
	@echo "  serve    – run a live-reload development server at http://localhost:1111"
	@echo "  build    – build the static site into ./public"
	@echo "  clean    – remove the ./public directory"

# Install zola using Cargo if not present
.PHONY: install
install:
	@if [ -x "$(ZOLA)" ]; then \
		echo "✅ Zola already installed at $(ZOLA)"; \
		exit 0; \
	fi
	@echo "⚙️  Zola not found – attempting installation..."
	@if [ -n "$(BREW)" ]; then \
		echo "🏺 Installing with Homebrew"; \
		brew install zola || true; \
	fi
	@if [ ! -x "$(shell command -v zola 2>/dev/null)" ] && [ -n "$(CARGO)" ]; then \
		echo "🦀 Installing with Cargo"; \
		cargo install zola --locked --version $(ZOLA_VERSION); \
	fi
	@if [ ! -x "$(shell command -v zola 2>/dev/null)" ]; then \
		echo "⚠️  Automatic install failed."; \
		echo "👉 Please follow the instructions at https://www.getzola.org/documentation/getting-started/installation/"; \
		exit 1; \
	fi
	@echo "✅ Zola installed successfully"

# Build the site (static HTML in ./public)
.PHONY: build
build:
	zola build

# Serve the site with live reload
.PHONY: serve
serve:
	zola serve --interface 127.0.0.1 --port 1111

# Remove generated static files
.PHONY: clean
clean:
	rm -rf public
	@echo "🗑️  Cleaned ./public directory" 