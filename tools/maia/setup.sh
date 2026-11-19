#!/bin/sh
set -eu

if [ "$#" -ne 1 ]; then
    echo "Usage: sh setup.sh /absolute/path/to/database-directory/maia" >&2
    exit 2
fi

maia_runtime=$1
mkdir -p "$maia_runtime"
uv venv --python 3.12 --allow-existing "$maia_runtime/venv"
uv pip install --python "$maia_runtime/venv/bin/python" 'maia2==0.11.0' 'torch==2.8.0'
"$maia_runtime/venv/bin/python" -I -c '
import sys
from maia2 import model
model.from_pretrained("rapid", device="cpu", save_root=sys.argv[1])
' "$maia_runtime/weights"
