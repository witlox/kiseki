#!/usr/bin/env bash
# Generate Python gRPC stubs from canonical proto files.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
PROTO_ROOT="$WORKSPACE_ROOT/specs/architecture/proto"
OUT_DIR="$SCRIPT_DIR/proto"

mkdir -p "$OUT_DIR"

# Use the project venv's grpc_tools
PYTHON="${WORKSPACE_ROOT}/.venv/bin/python"

"$PYTHON" -m grpc_tools.protoc \
    --proto_path="$PROTO_ROOT" \
    --python_out="$OUT_DIR" \
    --grpc_python_out="$OUT_DIR" \
    "kiseki/v1/common.proto" \
    "kiseki/v1/log.proto" \
    "kiseki/v1/key.proto"

# Create __init__.py for the generated package
mkdir -p "$OUT_DIR/kiseki/v1"
touch "$OUT_DIR/__init__.py"
touch "$OUT_DIR/kiseki/__init__.py"
touch "$OUT_DIR/kiseki/v1/__init__.py"

echo "Proto stubs generated in $OUT_DIR/"
