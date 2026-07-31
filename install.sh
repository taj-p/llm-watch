#!/usr/bin/env sh

set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_dir=${HOME}/.local/bin
target=${target_dir}/llm-watch
source=${repo_dir}/target/release/llm-watch

if ! command -v cargo >/dev/null 2>&1; then
  printf 'llm-watch: cargo is required; install Rust from https://rustup.rs\n' >&2
  exit 1
fi

cargo build --release --locked --manifest-path "$repo_dir/Cargo.toml"
mkdir -p "$target_dir"

if [ -e "$target" ] && [ ! -L "$target" ]; then
  printf 'llm-watch: refusing to replace existing file: %s\n' "$target" >&2
  exit 1
fi

ln -sfn "$source" "$target"
printf 'llm-watch: installed %s -> %s\n' "$target" "$source"
