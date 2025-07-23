#!/bin/bash

# Miri testing script for LocalCell project
# This script helps run Miri with proper configurations and exclusions

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Function to print colored output
print_status() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Check if nightly toolchain is available
if ! rustup toolchain list | grep -q nightly; then
    print_error "Nightly toolchain not found. Installing..."
    rustup toolchain install nightly
fi

# Check if miri is installed
if ! rustup +nightly component list --installed | grep -q miri; then
    print_status "Installing Miri..."
    rustup +nightly component add miri
fi

# Set Miri flags for better debugging
export MIRIFLAGS="-Zmiri-backtrace=full -Zmiri-strict-provenance"

# Function to run lib tests (excluding async)
run_lib_tests() {
    print_status "Running library tests with Miri (excluding async tests)..."
    cargo +nightly miri test --lib -- \
        --skip test_async_no_await_in_closure \
        --skip test_async_shared_state
}

# Function to run specific test
run_specific_test() {
    local test_name=$1
    print_status "Running specific test: $test_name"
    cargo +nightly miri test --lib "$test_name"
}

# Function to run SmolQueue tests only
run_smolqueue_tests() {
    print_status "Running SmolQueue tests with Miri..."
    cargo +nightly miri test --lib smol_q::tests
}

# Function to run LocalCell tests only
run_localcell_tests() {
    print_status "Running LocalCell tests with Miri..."
    cargo +nightly miri test --lib tests:: -- \
        --skip test_async_no_await_in_closure \
        --skip test_async_shared_state
}

# Function to run with extra strict checking
run_strict_tests() {
    print_status "Running tests with extra strict Miri checking..."
    export MIRIFLAGS="$MIRIFLAGS -Zmiri-tag-tracking"
    run_lib_tests
}

# Function to show usage
show_usage() {
    echo "Usage: $0 [OPTION]"
    echo "Run Miri tests for LocalCell project with proper exclusions"
    echo ""
    echo "Options:"
    echo "  lib              Run all library tests (default)"
    echo "  smolqueue        Run only SmolQueue tests"
    echo "  localcell        Run only LocalCell tests"
    echo "  strict           Run with extra strict checking"
    echo "  test <name>      Run specific test by name"
    echo "  help             Show this help message"
    echo ""
    echo "Examples:"
    echo "  $0                                    # Run all lib tests"
    echo "  $0 smolqueue                         # Run SmolQueue tests only"
    echo "  $0 strict                            # Run with strict checking"
    echo "  $0 test test_count_during_mutations  # Run specific test"
}

# Main script logic
case "${1:-lib}" in
    "lib")
        run_lib_tests
        ;;
    "smolqueue")
        run_smolqueue_tests
        ;;
    "localcell")
        run_localcell_tests
        ;;
    "strict")
        run_strict_tests
        ;;
    "test")
        if [ -z "$2" ]; then
            print_error "Test name required for 'test' option"
            show_usage
            exit 1
        fi
        run_specific_test "$2"
        ;;
    "help"|"-h"|"--help")
        show_usage
        ;;
    *)
        print_error "Unknown option: $1"
        show_usage
        exit 1
        ;;
esac

if [ $? -eq 0 ]; then
    print_status "Miri tests completed successfully!"
else
    print_error "Miri tests failed!"
    exit 1
fi
