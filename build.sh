#!/bin/sh
# PAIG build wrapper. POSIX sh only: no cargo, no crates.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
BIN="$ROOT/build/paig"
ORCH="$ROOT/build/paig-build"
RUSTC=${RUSTC:-rustc}

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ] && [ "${TERM:-}" != dumb ]; then
    RESET=$(printf '\033[0m')
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    GREEN=$(printf '\033[1;32m')
    CYAN=$(printf '\033[1;36m')
    BLUE=$(printf '\033[1;34m')
    YELLOW=$(printf '\033[1;33m')
    MAGENTA=$(printf '\033[1;35m')
    I_BUILD=$(printf '\357\206\262')
    I_CHECK=$(printf '\357\201\230')
    I_RUN=$(printf '\357\201\213')
    I_CLEAN=$(printf '\357\207\270')
    I_BENCH=$(printf '\357\203\244')
    I_HELP=$(printf '\357\201\231')
    I_TESTS=$(printf '\357\203\203')
    I_BINARY=$(printf '\357\204\240')
else
    RESET= BOLD= DIM= GREEN= CYAN= BLUE= YELLOW= MAGENTA=
    I_BUILD='[+]'
    I_CHECK='[ok]'
    I_RUN='[>]'
    I_CLEAN='[-]'
    I_BENCH='[~]'
    I_HELP='[?]'
    I_TESTS='[T]'
    I_BINARY='[B]'
fi

usage() {
    printf '%sPAIG build wrapper%s\n\n' "$BOLD" "$RESET"
    printf '%susage:%s ./build.sh [COMMAND]\n\n' "$DIM" "$RESET"
    printf '%scommands:%s\n' "$BOLD" "$RESET"
    printf '  %s%s%s  %-10s %s\n' "$GREEN" "$I_BUILD" "$RESET" "(none)" "build incrementally (G1/G2/G4) -> build/paig"
    printf '  %s%s%s  %-10s %s\n' "$CYAN" "$I_CHECK" "$RESET" "--check" "type-check only -> build/paig.rmeta"
    printf '  %s%s%s  %-10s %s\n' "$GREEN" "$I_RUN" "$RESET" "run" "build, then run the server in the foreground"
    printf '  %s%s%s  %-10s %s\n' "$YELLOW" "$I_CLEAN" "$RESET" "clean" "remove build/paig and intermediates"
    printf '  %s%s%s  %-10s %s\n' "$MAGENTA" "$I_BENCH" "$RESET" "bench" "build, then run tests/bench.sh"
    printf '  %s%s%s  %-10s %s\n' "$BLUE" "$I_HELP" "$RESET" "help" "show this help"
    printf '\n%sentrypoints:%s\n' "$BOLD" "$RESET"
    printf '  %s%s%s  %-10s %s\n' "$MAGENTA" "$I_TESTS" "$RESET" "tests" "./tests/test.sh"
    printf '  %s%s%s  %-10s %s\n' "$BLUE" "$I_BINARY" "$RESET" "binary" "./build/paig --help"
    printf '\n%senvironment:%s\n' "$BOLD" "$RESET"
    printf '  %-16s %s\n' "PAIG_BUILD_JOBS" "cap Mbed TLS C workers (default: nproc)"
    printf '  %-16s %s\n' "CC / AR" "C compiler and archiver (default: cc / ar)"
    printf '\n%sexamples:%s\n' "$BOLD" "$RESET"
    printf '  ./build.sh\n'
    printf '  ./build.sh run\n'
    printf '  ./tests/test.sh\n'
}

ensure_orchestrator() {
    # tools/build.rs derives the repo root from build/paig-build, so keep it here.
    if [ ! -x "$ORCH" ] || [ -n "$(find "$ROOT/tools/build.rs" "$ROOT/src/support/rules.rs" -newer "$ORCH" -print 2>/dev/null)" ]; then
        mkdir -p "$ROOT/build"
        "$RUSTC" --edition 2024 -C opt-level=2 -C strip=symbols -C panic=abort \
            -o "$ORCH" "$ROOT/tools/build.rs"
    fi
}

cmd=${1:-}
case "$cmd" in
    '')
        ensure_orchestrator
        exec "$ORCH"
        ;;
    --check)
        ensure_orchestrator
        exec "$ORCH" --check
        ;;
    run)
        shift
        ensure_orchestrator
        "$ORCH"
        exec "$BIN" "$@"
        ;;
    clean)
        ensure_orchestrator
        exec "$ORCH" clean
        ;;
    bench)
        exec sh "$ROOT/tests/bench.sh"
        ;;
    -h | --help | help)
        usage
        ;;
    *)
        printf 'build.sh: unknown command: %s\n\n' "$cmd" >&2
        usage >&2
        exit 2
        ;;
esac
