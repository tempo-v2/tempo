#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
cargo test --locked --workspace
npm --prefix ui ci
npm --prefix ui run build
uv run --project tools/python --python 3.12.12 --locked --exact python -m unittest discover -s tools -p 'test_*.py'
uv run --project tools/python --python 3.12.12 --locked --exact python -m unittest discover -s tools/lichess-packs
