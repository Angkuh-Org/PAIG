#!/bin/sh
# Unit tests, then the local mock-upstream integration test.
set -eu
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
"$ROOT/build.sh" >/dev/null
exec "$ROOT/build/paig-build" test "$@"
