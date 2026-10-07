#!/usr/bin/env bash
set -euo pipefail
umask 077
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
if [[ ! -t 0 || ! -t 1 ]]; then
  echo 'Owner interactive terminal required. Do not pass credentials as arguments or redirected input.' >&2
  exit 64
fi
if [[ ! -x "$repo_dir/local/kakao-loco-migration/bin/keychain-helper" ]]; then
  echo 'Run scripts/kakao-loco-prepare.sh first.' >&2
  exit 69
fi
exec bun "$repo_dir/sidecar/kakao/onboarding.ts" login "$@"
