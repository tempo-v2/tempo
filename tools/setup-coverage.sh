#!/bin/sh
set -eu
runtime="$HOME/Library/Application Support/Tempo Lean/Maia"
mkdir -p "$runtime/weights"
uv venv --allow-existing --python 3.12 "$runtime/venv"
uv pip install --python "$runtime/venv/bin/python" 'maia2==0.11.0' 'torch==2.8.0'
"$runtime/venv/bin/python" - "$runtime/weights" <<'PY'
import sys
from maia2 import model
model.from_pretrained("rapid", device="cpu", save_root=sys.argv[1])
PY
