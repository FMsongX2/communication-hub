#!/usr/bin/env python3
"""Read nonsecret paths/metadata only; never access Keychain or Kakao caches."""
import json
import os
import subprocess
from pathlib import Path

root = Path(__file__).resolve().parent.parent
config_path = Path.home() / "Library/Application Support/CommunicationHub/config.json"
candidate = Path.home() / ".communication-hub/kakao-loco/sidecar.shadow.json"
current = json.loads(config_path.read_text()) if config_path.exists() else {}
probe = subprocess.run(["launchctl", "print", f"gui/{os.getuid()}/dev.communication-hub.kakao-loco"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
result = {
    "live_config_exists": config_path.exists(),
    "live_transport": "loco" if current.get("kakao", {}).get("loco") else "ax",
    "live_dispatch_enabled": current.get("dispatch_enabled"),
    "live_socket_exists": Path(current["socket"]).exists() if current.get("socket") else False,
    "sidecar_candidate_exists": candidate.exists(),
    "sidecar_launchagent_loaded": probe.returncode == 0,
    "keychain_helper_compiled": (root / "local/kakao-loco-migration/bin/keychain-helper").is_file(),
    "keychain_accessed": False,
    "credential_presence": "not_inspected",
    "login_attempted_by_readiness": False,
    "safe_next_action": "Owner runs scripts/kakao-loco-onboard.sh in an interactive terminal after preparation",
}
print(json.dumps(result, indent=2))
