#!/usr/bin/env python3
"""Owner-side read-only socket inspection. Deliberately has no send method."""
import argparse
import json
import socket
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("method", choices=["status", "list_rooms"])
parser.add_argument("--config", type=Path, default=Path.home() / ".communication-hub/kakao-loco/sidecar.shadow.json")
args = parser.parse_args()
config = json.loads(args.config.read_text())
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.settimeout(30)
    client.connect(config["socket_path"])
    client.sendall((json.dumps({"id": "owner-readonly", "method": args.method, "params": {}}) + "\n").encode())
    result = client.makefile("rb").readline(1024 * 1024)
    if not result.endswith(b"\n"):
        raise SystemExit("Sidecar response exceeded limit or ended unexpectedly")
    response = json.loads(result)
    if response.get("id") != "owner-readonly":
        raise SystemExit("Sidecar response ID mismatch")
    print(json.dumps(response, ensure_ascii=False, indent=2))
