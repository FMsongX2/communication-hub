#!/usr/bin/env bash
# Offline integration preparation. Does not login, access Keychain, or load services.
set -euo pipefail
umask 077
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
staging_dir="$repo_dir/local/kakao-loco-migration"
command -v bun >/dev/null
command -v swiftc >/dev/null
mkdir -p "$staging_dir/bin"
cd "$repo_dir/third_party/agent-messenger"
bun install --frozen-lockfile --ignore-scripts
cd "$repo_dir/sidecar/kakao"
bun install --frozen-lockfile --ignore-scripts
bun run verify-vendor
bun run typecheck
bun test
bun test "$repo_dir/scripts/kakao-loco-onboarding.test.ts"
swiftc "$repo_dir/scripts/kakao-loco-keychain.swift" -o "$staging_dir/bin/keychain-helper.next" -framework Security
"$staging_dir/bin/keychain-helper.next" --self-test
mv "$staging_dir/bin/keychain-helper.next" "$staging_dir/bin/keychain-helper"
export KAKAO_LOCO_REPO_DIR="$repo_dir"
export KAKAO_LOCO_BUN_PATH="$(command -v bun)"
python3 - <<'PY'
import os, pathlib, plistlib
root = pathlib.Path(os.environ['KAKAO_LOCO_REPO_DIR'])
stage = root / 'local/kakao-loco-migration'
owner = pathlib.Path.home() / '.communication-hub/kakao-loco'
plist = {
  'Label': 'dev.communication-hub.kakao-loco',
  'ProgramArguments': [os.environ['KAKAO_LOCO_BUN_PATH'], str(root / 'sidecar/kakao/main.ts'), '--config', str(owner / 'sidecar.shadow.json')],
  'WorkingDirectory': str(root),
  'EnvironmentVariables': {'KAKAO_LOCO_KEYCHAIN_HELPER': str(stage / 'bin/keychain-helper')},
  'RunAtLoad': False, 'KeepAlive': False, 'ThrottleInterval': 30,
  'StandardOutPath': str(owner / 'sidecar.stdout.log'),
  'StandardErrorPath': str(owner / 'sidecar.stderr.log'),
  'Umask': 0o077,
}
target = stage / 'dev.communication-hub.kakao-loco.plist'
with target.open('xb') if not target.exists() else target.open('wb') as f:
    plistlib.dump(plist, f)
os.chmod(target, 0o600)
print('LaunchAgent candidate prepared; not copied to LaunchAgents or loaded.')
PY
echo 'Preparation complete. Owner can run scripts/kakao-loco-onboard.sh in their own terminal.'
