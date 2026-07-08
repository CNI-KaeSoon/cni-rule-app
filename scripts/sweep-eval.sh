#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROJECT_ROOT="$(cd "$ROOT/.." && pwd)"
OUT_DIR="${OUT_DIR:-/tmp/cni-rag-phase2/sweep}"
VECTOR_CACHE_DIR="${CNI_RULES_VECTOR_CACHE_DIR:-$OUT_DIR/vector-cache}"
VECTOR_MODEL_DIR="${CNI_RULES_FASTEMBED_MODEL_DIR:-}"

mkdir -p "$OUT_DIR"

CNI_PACK="$PROJECT_ROOT/04_data/90_index-build/pack-cni-2026-02-27"
CTP_PACK="$PROJECT_ROOT/04_data/90_index-build/ctp/pack-ctp-2025-12-23"
CIHC_PACK="$PROJECT_ROOT/04_data/90_index-build/cihc/pack-cihc-2026-03-17"
IKCC_PACK="$PROJECT_ROOT/04_data/90_index-build/ikcc/pack-ikcc-2026-06-04"
EVAL_DIR="$PROJECT_ROOT/01_docs/eval"
SUMMARY="$OUT_DIR/vector-sweep-summary.tsv"

eval_args=(
  --vectors
  --vector-cache "$VECTOR_CACHE_DIR"
)
if [[ -n "$VECTOR_MODEL_DIR" ]]; then
  eval_args+=(--vector-model-dir "$VECTOR_MODEL_DIR")
fi

printf 'rrf_k\tvector_weight\tgolden\tcases\thit_at_5\tpin_hit_at_5\tretrieval_hit_at_5\tp95_us\tstdout\tstderr\n' > "$SUMMARY"

run_eval() {
  local rrf_k="$1"
  local weight="$2"
  local name="$3"
  local golden="$4"
  shift 4

  local slug="${name}-rrf${rrf_k}-w${weight}"
  local stdout="$OUT_DIR/${slug}.tsv"
  local stderr="$OUT_DIR/${slug}.stderr"

  cargo run --release -p rules-core --features vectors --example eval -- \
    --golden "$golden" \
    "${eval_args[@]}" \
    --rrf-k "$rrf_k" \
    --vector-weight "$weight" \
    "$@" \
    > "$stdout" 2> "$stderr"

  local summary routes p95 cases hit pin retrieval
  summary="$(grep '^summary cases=' "$stdout" | tail -1)"
  routes="$(grep '^routes ' "$stdout" | tail -1)"
  p95="$(grep '^p95_us=' "$stderr" | tail -1 | cut -d= -f2)"
  cases="$(awk '{for (i=1;i<=NF;i++) if ($i ~ /^cases=/) {sub("cases=","",$i); print $i}}' <<<"$summary")"
  hit="$(awk '{for (i=1;i<=NF;i++) if ($i ~ /^hit@5=/) {sub("hit@5=","",$i); print $i}}' <<<"$summary")"
  pin="$(awk '{for (i=1;i<=NF;i++) if ($i ~ /^pin_hit@5=/) {sub("pin_hit@5=","",$i); print $i}}' <<<"$routes")"
  retrieval="$(awk '{for (i=1;i<=NF;i++) if ($i ~ /^retrieval_hit@5=/) {sub("retrieval_hit@5=","",$i); print $i}}' <<<"$routes")"

  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$rrf_k" "$weight" "$name" "$cases" "$hit" "$pin" "$retrieval" "$p95" "$stdout" "$stderr" \
    >> "$SUMMARY"
}

for rrf_k in 20 60 100; do
  for weight in 0.5 1.0 2.0; do
    run_eval "$rrf_k" "$weight" golden "$EVAL_DIR/golden.jsonl" --rules "$CNI_PACK"
    run_eval "$rrf_k" "$weight" golden-cni-hard "$EVAL_DIR/golden-cni-hard.jsonl" --rules "$CNI_PACK"
    run_eval "$rrf_k" "$weight" golden-ctp "$EVAL_DIR/golden-ctp.jsonl" --institution ctp --rules "$CTP_PACK"
    run_eval "$rrf_k" "$weight" golden-cihc "$EVAL_DIR/golden-cihc.jsonl" --institution cihc --rules "$CIHC_PACK"
    run_eval "$rrf_k" "$weight" golden-ikcc "$EVAL_DIR/golden-ikcc.jsonl" --institution ikcc --rules "$IKCC_PACK"
    run_eval "$rrf_k" "$weight" golden-multipack "$EVAL_DIR/golden-multipack.jsonl" \
      --pack cni="$CNI_PACK" \
      --pack ctp="$CTP_PACK" \
      --pack cihc="$CIHC_PACK" \
      --pack ikcc="$IKCC_PACK"
  done
done

echo "$SUMMARY"
