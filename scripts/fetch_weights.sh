#!/usr/bin/env bash
#
# Fetch the uibk-embedding-v1 weights and tokenizer into embedder/weights/.
#
# The files are pinned to a single model revision. Every file is verified against
# embedder/weights/SHA256SUMS, so a partial download or an upstream retag fails loudly
# instead of producing an engine that loads and returns nonsense.
#
# Idempotent: a file whose checksum already matches is not downloaded again.
#
# Usage:
#   scripts/fetch_weights.sh              # fetch and verify
#   scripts/fetch_weights.sh --write-sums # regenerate SHA256SUMS from what is on disk
#   scripts/fetch_weights.sh --force      # re-download everything

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL_REPO="${MODEL_REPO:-DataScience-UIBK/uibk-embedding-v1}"
MODEL_REVISION="${MODEL_REVISION:-b18df82e77306006a1dd0162199f6dfeacb5492e}"
DEST="${ADRIAN_WEIGHTS_DIR:-$REPO_ROOT/embedder/weights}"
SUMS="$DEST/SHA256SUMS"

FILES=(
  model.safetensors
  dense_branch.safetensors
  1_HybridTokens/model.safetensors
  tokenizer.json
  tokenizer_config.json
  config.json
  config_sentence_transformers.json
  sentence_bert_config.json
)

write_sums=false
force=false
for arg in "$@"; do
  case "$arg" in
    --write-sums) write_sums=true ;;
    --force) force=true ;;
    -h|--help)
      sed -n '2,20p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      echo "unknown argument: $arg" >&2
      exit 2
      ;;
  esac
done

mkdir -p "$DEST"

verify() {
  local name="$1"
  local expected
  expected="$(awk -v f="$name" '$2 == f || $2 == "*"f { print $1 }' "$SUMS" | head -n 1)"
  if [[ -z "$expected" ]]; then
    echo "  $name: no entry in SHA256SUMS" >&2
    return 1
  fi
  local actual
  actual="$(sha256sum "$DEST/$name" | awk '{ print $1 }')"
  if [[ "$actual" != "$expected" ]]; then
    echo "  $name: checksum mismatch" >&2
    echo "    expected $expected" >&2
    echo "    actual   $actual" >&2
    return 1
  fi
  return 0
}

if $write_sums; then
  echo "Writing $SUMS"
  : > "$SUMS"
  for file in "${FILES[@]}"; do
    if [[ ! -f "$DEST/$file" ]]; then
      echo "  missing $file; run without --write-sums first" >&2
      exit 1
    fi
    printf '%s  %s\n' "$(sha256sum "$DEST/$file" | awk '{ print $1 }')" "$file" >> "$SUMS"
    echo "  $file"
  done
  echo "Done. Commit $SUMS so image builds stay reproducible."
  exit 0
fi

if [[ ! -f "$SUMS" ]]; then
  echo "error: $SUMS does not exist." >&2
  echo "Fetch once without a checksum file, then record it:" >&2
  echo "  scripts/fetch_weights.sh --no-verify   # (not supported: use the two-step below)" >&2
  echo "  1. MODEL_NO_VERIFY=1 scripts/fetch_weights.sh" >&2
  echo "  2. scripts/fetch_weights.sh --write-sums" >&2
  echo "  3. scripts/fetch_weights.sh            # now verifies" >&2
  if [[ "${MODEL_NO_VERIFY:-0}" != "1" ]]; then
    exit 1
  fi
fi

base="https://huggingface.co/${MODEL_REPO}/resolve/${MODEL_REVISION}"

for file in "${FILES[@]}"; do
  target="$DEST/$file"
  mkdir -p "$(dirname "$target")"
  if [[ -f "$target" ]] && ! $force && [[ -f "$SUMS" ]] && verify "$file" 2>/dev/null; then
    echo "  $file: present and verified"
    continue
  fi
  echo "  $file: downloading"
  curl -fL --retry 5 --retry-delay 3 --progress-bar -o "$target.tmp" "$base/$file"
  mv "$target.tmp" "$target"
done

if [[ -f "$SUMS" ]] && [[ "${MODEL_NO_VERIFY:-0}" != "1" ]]; then
  echo "Verifying against $SUMS"
  ( cd "$DEST" && sha256sum -c "$SUMS" )
  echo "All model files verified."
else
  echo "Downloaded without verification. Record checksums with:"
  echo "  scripts/fetch_weights.sh --write-sums"
fi
