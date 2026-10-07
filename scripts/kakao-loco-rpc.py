#!/usr/bin/env python3
"""Owner-side read-only socket inspection. Deliberately has no send method."""
import argparse
import json
import socket
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("method", choices=["status", "list_rooms", "room_info", "message_page"])
parser.add_argument("--config", type=Path, default=Path.home() / ".communication-hub/kakao-loco/sidecar.shadow.json")
parser.add_argument("--chat-id")
parser.add_argument("--from", dest="from_cursor", default="0")
parser.add_argument("--count", type=int, default=20)
args = parser.parse_args()
params = {}
if args.method in {"room_info", "message_page"}:
    if not args.chat_id:
        parser.error("--chat-id is required for room diagnostics")
    params["chat_id"] = args.chat_id
if args.method == "message_page":
    if not 1 <= args.count <= 20:
        parser.error("--count must be 1..20")
    params.update({"from": args.from_cursor, "count": args.count})
config = json.loads(args.config.read_text())
with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as client:
    client.settimeout(30)
    client.connect(config["socket_path"])
    client.sendall((json.dumps({"id": "owner-readonly", "method": args.method, "params": params}) + "\n").encode())
    result = client.makefile("rb").readline(1024 * 1024)
    if not result.endswith(b"\n"):
        raise SystemExit("Sidecar response exceeded limit or ended unexpectedly")
    response = json.loads(result)
    if response.get("id") != "owner-readonly":
        raise SystemExit("Sidecar response ID mismatch")
    print(json.dumps(response, ensure_ascii=False, indent=2))
