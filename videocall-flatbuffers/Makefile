SHELL := /bin/bash

# Directories
SCHEMA_DIR := schemas
OUTPUT_DIR := src/generated

# Find all .fbs files
SCHEMAS := $(wildcard $(SCHEMA_DIR)/*.fbs)

.PHONY: all
all: generate

.PHONY: check-flatc
check-flatc:
	@which flatc > /dev/null || (echo "flatc not found. Installing..." && $(MAKE) install-flatc)

.PHONY: install-flatc
install-flatc:
	@echo "Installing flatc compiler..."
	@if [ -f /etc/debian_version ]; then \
		sudo apt-get update && sudo apt-get install -y flatbuffers-compiler; \
	elif [ -f /etc/redhat-release ]; then \
		sudo yum install -y flatbuffers-compiler; \
	elif [ "$(shell uname)" = "Darwin" ]; then \
		brew install flatbuffers; \
	else \
		echo "Please install flatc manually from https://github.com/google/flatbuffers/releases"; \
		exit 1; \
	fi

.PHONY: generate
generate: check-flatc
	@echo "Generating Rust code from FlatBuffer schemas..."
	@mkdir -p $(OUTPUT_DIR)
	@flatc --rust --gen-mutable --gen-object-api -o $(OUTPUT_DIR) $(SCHEMAS)
	@echo "Generated Rust code in $(OUTPUT_DIR)/"
	@echo "Creating unified module..."
	@echo "// Auto-generated unified module" > $(OUTPUT_DIR)/mod.rs
	@for file in $(OUTPUT_DIR)/*_generated.rs; do \
		base=$$(basename $$file _generated.rs); \
		echo "#[path = \"$$(basename $$file)\"]" >> $(OUTPUT_DIR)/mod.rs; \
		echo "pub mod $${base};" >> $(OUTPUT_DIR)/mod.rs; \
	done

.PHONY: clean
clean:
	@echo "Cleaning generated files..."
	@rm -rf $(OUTPUT_DIR)

.PHONY: rebuild
rebuild: clean generate
