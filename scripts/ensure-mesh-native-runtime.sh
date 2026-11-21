#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BACKEND="${1:-}"
if [[ -z "$BACKEND" ]]; then
  case "$(uname -s)" in
    Darwin) BACKEND="metal" ;;
    *) BACKEND="cpu" ;;
  esac
fi

CACHE_DIR="${MESH_LLM_NATIVE_RUNTIME_CACHE_DIR:-$ROOT/.cache/mesh-llm-native-runtime}"
OUT_DIR="${MESH_LLM_NATIVE_RUNTIME_OUT_DIR:-$ROOT/.cache/mesh-llm-native-runtime-artifacts}"

metadata="$($ROOT/bin/cargo metadata --manifest-path "$ROOT/desktop/src-tauri/Cargo.toml" --features mesh-llm --format-version 1)"
SDK_MANIFEST="$(python3 -c 'import json,sys; data=json.load(sys.stdin); print(next(p["manifest_path"] for p in data["packages"] if p["name"]=="mesh-llm-sdk"))' <<<"$metadata")"
MESH_ROOT="$(git -C "$(dirname "$SDK_MANIFEST")" rev-parse --show-toplevel)"
RUNTIME_VERSION="$(cat "$MESH_ROOT/skippy/crates/skippy-native-runtime/RUNTIME_VERSION")"

case "$(uname -s)/$(uname -m)/$BACKEND" in
  Darwin/arm64/metal) RUNTIME_ID="meshllm-native-runtime-darwin-aarch64-metal" ;;
  Darwin/x86_64/metal) RUNTIME_ID="meshllm-native-runtime-darwin-x86_64-metal" ;;
  */*/cpu)
    case "$(uname -s)/$(uname -m)" in
      Darwin/arm64) RUNTIME_ID="meshllm-native-runtime-darwin-aarch64-cpu" ;;
      Darwin/x86_64) RUNTIME_ID="meshllm-native-runtime-darwin-x86_64-cpu" ;;
      Linux/x86_64) RUNTIME_ID="meshllm-native-runtime-linux-x86_64-cpu" ;;
      Linux/aarch64) RUNTIME_ID="meshllm-native-runtime-linux-aarch64-cpu" ;;
      *) RUNTIME_ID="" ;;
    esac
    ;;
  *) RUNTIME_ID="" ;;
esac

if [[ -n "$RUNTIME_ID" && -f "$CACHE_DIR/$RUNTIME_VERSION/$RUNTIME_ID/manifest.json" ]]; then
  printf '%s\n' "$CACHE_DIR"
  exit 0
fi

echo "Preparing MeshLLM native runtime ($BACKEND) release $RUNTIME_VERSION..." >&2
runtime_dir="$(cd "$MESH_ROOT" && scripts/ci-prepare-native-runtime.sh "$OUT_DIR" "$BACKEND")"
(
  cd "$MESH_ROOT"
  REPO_ROOT="$MESH_ROOT"
  source scripts/lib/automation.sh
  mesh_automation automation runtime-cache-install "$runtime_dir" "$CACHE_DIR" >&2
)
printf '%s\n' "$CACHE_DIR"
