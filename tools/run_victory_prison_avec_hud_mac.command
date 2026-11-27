#!/bin/bash
set -eu
root="$(cd "$(dirname "$0")" && pwd)"
exec python3 "$root/macos/run.py" --prison-peau "$@"
