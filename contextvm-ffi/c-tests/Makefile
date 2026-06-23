# Makefile for ContextVM FFI C Tests
#
# Usage:
#   make          - Build the test program
#   make test     - Build and run tests
#   make clean    - Clean build artifacts

# Configuration
CARGO_FLAGS ?= --release
BUILD_MODE ?= release

# Detect target directory based on build mode
ifeq ($(BUILD_MODE),debug)
    TARGET_DIR = ../../target/debug
    CARGO_FLAGS =
else
    TARGET_DIR = ../../target/release
endif

# Compiler settings
CC = gcc
CFLAGS = -Wall -Wextra -g -I../headers
LDFLAGS = -L$(TARGET_DIR) -lcontextvm_ffi -lpthread -ldl -lm

# macOS specific settings (if needed)
ifeq ($(shell uname),Darwin)
    LDFLAGS += -framework Security -framework CoreFoundation
endif

# Targets
TEST_BIN = test_ffi

.PHONY: all build test clean ffi

all: build

# Build the Rust FFI library
ffi:
	cd .. && cargo build $(CARGO_FLAGS)

# Build the C test program
build: ffi
	@echo "Building C test program ($(BUILD_MODE) mode)..."
	$(CC) $(CFLAGS) -o $(TEST_BIN) test_ffi.c $(LDFLAGS)

# Run the tests
test: build
	@echo "\n========================================"
	@echo "Running C FFI Tests"
	@echo "========================================\n"
	@LD_LIBRARY_PATH=$(TARGET_DIR) ./$(TEST_BIN)

# Clean build artifacts
clean:
	rm -f $(TEST_BIN)
	cd .. && cargo clean

# Build in debug mode for debugging
debug:
	$(MAKE) BUILD_MODE=debug

# Help target
help:
	@echo "ContextVM FFI C Tests Makefile"
	@echo ""
	@echo "Targets:"
	@echo "  make          - Build the Rust FFI library and C test program (release mode)"
	@echo "  make test     - Build and run the C tests"
	@echo "  make debug    - Build in debug mode (with debug symbols)"
	@echo "  make clean    - Clean all build artifacts"
	@echo "  make help     - Show this help message"
	@echo ""
	@echo "Environment Variables:"
	@echo "  BUILD_MODE    - 'release' (default) or 'debug'"
