#!/usr/bin/env bash
#
# fetch-sst-corpus.sh — download SingleStepTests CPU vectors for genesoxide.
#
# This script is DOCUMENTATION / TOOLING. It is NOT run automatically by the
# build or the test suite. It exists so the vendored subset can be regenerated
# and so the opt-in full corpus can be fetched on demand.
#
# Two modes:
#
#   (default, "vendored")   Re-create the small COMMITTED subset:
#                             - z80: first 100 vectors of a curated opcode set,
#                               written as plain JSON to
#                               tests/z80-tests-vendored/v1/v1/<op>.json
#                             - m68k: first 150 vectors of a curated opcode set,
#                               converted to the binary .json.bin format at
#                               tests/m68000-tests/v1/<OP>.json.bin
#                           These directories ARE committed; this mode only needs
#                           to be run to refresh/extend them.
#
#   --full                  Fetch the ENTIRE upstream corpora (all opcodes, all
#                           vectors) into GITIGNORED directories used by the
#                           #[ignore] full_suite tests:
#                             - z80:  tests/z80-tests/v1/v1/<op>.json
#                             - m68k: tests/m68000-tests-full/v1/<OP>.json.bin
#                           These are large (z80 ~1.6M vectors, 68000 ~300k) and
#                           are never committed. Run, then:
#                             cargo test -p genesoxide-test-harness --test z80_suite  full_suite -- --ignored --nocapture
#                             cargo test -p genesoxide-test-harness --test m68k_suite full_suite -- --ignored --nocapture
#
# Sources:
#   z80    github.com/SingleStepTests/z80        (v1/<name>.json, uncompressed, MIT)
#   680x0  github.com/SingleStepTests/680x0      (68000/v1/<name>.json.gz, gzipped;
#                                                  NO explicit upstream LICENSE — see
#                                                  tests/m68000-tests/README.md)
#
# Requirements: bash, curl, python3, and (for m68k) the committed converter example
#   cargo run -p genesoxide-test-harness --example sst_json2bin
#
set -euo pipefail

# ── Locate repo root (this script lives in <root>/scripts) ────────────────
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

MODE="vendored"
if [[ "${1:-}" == "--full" ]]; then
    MODE="full"
fi

# Vendored truncation (ignored in --full mode). z80 JSON is far bulkier per
# vector than the packed m68k .json.bin, so it gets a smaller cap to keep the
# committed footprint in the ~3-4 MB budget while still running 100+ vectors/op.
M68K_MAX_VECTORS=150
Z80_MAX_VECTORS=100

Z80_RAW="https://raw.githubusercontent.com/SingleStepTests/z80/main/v1"
M68K_RAW="https://raw.githubusercontent.com/SingleStepTests/680x0/main/68000/v1"
Z80_API="https://api.github.com/repos/SingleStepTests/z80/contents/v1"
M68K_API="https://api.github.com/repos/SingleStepTests/680x0/contents/68000/v1"

# ── Curated vendored opcode sets (representative coverage) ────────────────
# z80: bare filenames WITHOUT the .json extension (spaces are literal here;
# they are %20-encoded for the raw URL below).
Z80_VENDORED=(
    "00" "01" "3e" "41" "80" "90" "a0" "a8" "b0" "b8"
    "09" "27" "c3" "18" "10" "cd" "c9" "c5" "c1"
    "cb 00" "cb 40" "cb c0" "ed 42" "ed 41" "ed b0"
    "dd 21" "dd cb __ 46"
)
# m68k: upstream opcode base names (converter appends .json.bin on output).
# NOTE: BTST/BSET (cycle timing), LINK (LINK A7 quirk) and DIVU (overflow N/Z)
# were previously omitted for known core discrepancies; those core bugs are now
# fixed, so the four opcodes are vendored and covered at 100%.
M68K_VENDORED=(
    "MOVE.b" "MOVE.l" "MOVEA.l" "MOVEM.l" "ADD.w" "ADDA.l" "SUB.w" "ADDX.w"
    "CMP.l" "AND.w" "OR.l" "EOR.w" "NOT.l" "ASL.w" "LSR.l" "ROXL.w"
    "Bcc" "DBcc" "JSR" "RTS" "PEA" "Scc" "SWAP"
    "EXG" "LEA" "TST.l" "CLR.w" "MULU" "ABCD"
    "BTST" "BSET" "LINK" "DIVU"
)

# URL-encode spaces (the only special char in these names) as %20.
urlenc() { printf '%s' "$1" | sed 's/ /%20/g'; }

tmpdir="$(mktemp -d)"
trap 'rm -rf "$tmpdir"' EXIT

convert_m68k() { # <in.json.gz> <out.json.bin> [max]
    local in="$1" out="$2" max="${3:-}"
    ( cd "$ROOT" && cargo run -q -p genesoxide-test-harness --example sst_json2bin -- "$in" "$out" $max )
}

# Truncate a JSON array file to its first N elements (in place) via python3.
truncate_json() { # <file> <n>
    python3 - "$1" "$2" <<'PY'
import json, sys
path, n = sys.argv[1], int(sys.argv[2])
with open(path) as f:
    data = json.load(f)
data = data[:n]
with open(path, "w") as f:
    json.dump(data, f)
PY
}

fetch_z80_vendored() {
    local outdir="$ROOT/tests/z80-tests-vendored/v1/v1"
    mkdir -p "$outdir"
    for op in "${Z80_VENDORED[@]}"; do
        local enc; enc="$(urlenc "$op")"
        local dst="$outdir/$op.json"
        echo "z80  $op.json"
        curl -fsSL "$Z80_RAW/$enc.json" -o "$dst"
        truncate_json "$dst" "$Z80_MAX_VECTORS"
    done
}

fetch_m68k_vendored() {
    local outdir="$ROOT/tests/m68000-tests/v1"
    mkdir -p "$outdir"
    for op in "${M68K_VENDORED[@]}"; do
        local enc; enc="$(urlenc "$op")"
        local gz="$tmpdir/$op.json.gz"
        echo "m68k $op.json.bin"
        curl -fsSL "$M68K_RAW/$enc.json.gz" -o "$gz"
        convert_m68k "$gz" "$outdir/$op.json.bin" "$M68K_MAX_VECTORS"
    done
}

# Enumerate every file in an upstream directory via the GitHub contents API.
list_api() { # <api-url>
    curl -fsSL "$1" | python3 -c 'import json,sys; [print(e["name"]) for e in json.load(sys.stdin) if e["type"]=="file"]'
}

fetch_z80_full() {
    local outdir="$ROOT/tests/z80-tests/v1/v1"   # gitignored
    mkdir -p "$outdir"
    echo "Enumerating full z80 corpus…"
    while IFS= read -r name; do
        [[ "$name" == *.json ]] || continue
        local base="${name%.json}"
        local enc; enc="$(urlenc "$base")"
        echo "z80  $name"
        curl -fsSL "$Z80_RAW/$enc.json" -o "$outdir/$name"
    done < <(list_api "$Z80_API")
}

fetch_m68k_full() {
    local outdir="$ROOT/tests/m68000-tests-full/v1"   # gitignored
    mkdir -p "$outdir"
    echo "Enumerating full 68000 corpus…"
    while IFS= read -r name; do
        [[ "$name" == *.json.gz ]] || continue
        local base="${name%.json.gz}"
        local enc; enc="$(urlenc "$base")"
        local gz="$tmpdir/$base.json.gz"
        echo "m68k $base.json.bin"
        curl -fsSL "$M68K_RAW/$enc.json.gz" -o "$gz"
        convert_m68k "$gz" "$outdir/$base.json.bin"
    done < <(list_api "$M68K_API")
}

case "$MODE" in
    vendored)
        echo "== vendored subset (committed; m68k=$M68K_MAX_VECTORS, z80=$Z80_MAX_VECTORS vectors/opcode) =="
        fetch_z80_vendored
        fetch_m68k_vendored
        ;;
    full)
        echo "== FULL corpus (gitignored, opt-in) =="
        fetch_z80_full
        fetch_m68k_full
        ;;
esac

echo "Done ($MODE)."
