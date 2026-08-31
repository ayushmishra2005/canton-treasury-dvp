#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
test -f LICENSE
test -f NOTICE
grep -q "Apache License" LICENSE
grep -q "OpenZeppelin Relayer 1.5.0" NOTICE
echo "LICENSE_OK"
