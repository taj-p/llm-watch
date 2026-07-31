#!/usr/bin/env sh

set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
target_dir=${HOME}/.local/bin
target=${target_dir}/llm-watch
source=${repo_dir}/bin/llm-watch

if ! command -v python3 >/dev/null 2>&1; then
  printf 'llm-watch: python3 is required\n' >&2
  exit 1
fi

python3 -c 'import sys; raise SystemExit(sys.version_info < (3, 9))' || {
  printf 'llm-watch: Python 3.9 or later is required\n' >&2
  exit 1
}

mkdir -p "$target_dir"

if [ -e "$target" ] && [ ! -L "$target" ]; then
  printf 'llm-watch: refusing to replace existing file: %s\n' "$target" >&2
  exit 1
fi

ln -sfn "$source" "$target"
printf 'llm-watch: installed %s -> %s\n' "$target" "$source"
