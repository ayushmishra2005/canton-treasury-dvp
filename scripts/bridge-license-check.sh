#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
test -f LICENSE
test -f NOTICE
grep -q "Apache License" LICENSE
grep -q "Apache-2.0" NOTICE
grep -q "OpenZeppelin Relayer 1.5.0" NOTICE
grep -q "AGPL-3.0" NOTICE
grep -q "MIT" NOTICE
grep -q "BSD-3-Clause-Clear" NOTICE
echo "LICENSE_OK"
