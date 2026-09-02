#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

python3 - <<'PY'
import json
import os
import re
import subprocess
import sys

SKIP_PREFIXES = (
    "bridge/target/",
    "solana/target/",
    "zama/node_modules/",
    "zama/artifacts/",
    "zama/cache/",
)
SKIP_FILES = {"scripts/bridge-secret-scan.sh"}
KEYPAIR_NAME = re.compile(r".*-keypair\.json$")
PRIVATE_NAME = re.compile(
    r"(private[-_]key|\.pem$|\.seed$|[-_]seed\.(txt|json)$)",
    re.IGNORECASE,
)
PEM_PRIVATE = re.compile(r"BEGIN (RSA |OPENSSH |EC |DSA )?PRIVATE KEY")
ASSIGNED_SECRET = re.compile(
    r"KEYSTORE_PASSPHRASE=[A-Za-z0-9]{8,}|RELAYER_API_KEY=[A-Za-z0-9]{16,}"
)


def listed_files():
    seen = set()
    commands = (
        ["git", "ls-files", "-z"],
        ["git", "ls-files", "-z", "--others", "--exclude-standard"],
    )
    for command in commands:
        raw = subprocess.check_output(command)
        for path in raw.split(b"\0"):
            if not path:
                continue
            text = path.decode("utf-8", "surrogateescape")
            if text not in seen:
                seen.add(text)
                yield text


def should_scan(path):
    base = os.path.basename(path)
    if base == ".env.example":
        return True
    if base == ".env" or base.startswith(".env."):
        return False
    if path in SKIP_FILES:
        return False
    return not path.startswith(SKIP_PREFIXES)


def is_solana_keypair(raw):
    try:
        data = json.loads(raw)
    except json.JSONDecodeError:
        return False
    if not isinstance(data, list) or len(data) != 64:
        return False
    return all(isinstance(item, int) and 0 <= item <= 255 for item in data)


hits = []
for path in listed_files():
    if not should_scan(path):
        continue
    reasons = []
    if KEYPAIR_NAME.search(os.path.basename(path)):
        reasons.append("solana keypair filename")
    if PRIVATE_NAME.search(os.path.basename(path)):
        reasons.append("private-key or seed filename")
    if os.path.isfile(path) and not os.path.islink(path):
        try:
            raw = open(path, "rb").read()
        except OSError:
            raw = b""
        text = raw.decode("utf-8", "replace")
        if is_solana_keypair(text):
            reasons.append("solana keypair json")
        if PEM_PRIVATE.search(text):
            reasons.append("pem private key")
        if ASSIGNED_SECRET.search(text):
            reasons.append("inline secret assignment")
    for reason in reasons:
        hits.append(f"{path}: {reason}")

if hits:
    sys.stderr.write("SECRET_SCAN_FAIL\n")
    sys.stderr.write("\n".join(hits) + "\n")
    raise SystemExit(1)
print("SECRET_SCAN_OK")
PY
