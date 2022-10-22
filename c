#!/bin/sh
export CARGO_TARGET_DIR=cargo-target
export RUST_BACKTRACE=1
cargo $*
