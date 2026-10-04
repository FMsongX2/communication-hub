#!/usr/bin/env bash
set -euo pipefail
if [[ $# -ne 2 || "$1" != "--model" ]]; then
    echo 'usage: setup-macos.sh --model SUPPORTED_MODEL_ID' >&2
    exit 64
fi
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
command -v jq >/dev/null || { echo 'Install jq before generating configuration.' >&2; exit 69; }
support_dir="$HOME/Library/Application Support/CommunicationHub"
if [[ -e "$support_dir/config.json" ]]; then
    echo 'Existing configuration found; refusing to overwrite it.' >&2
    exit 73
fi
umask 077
mkdir -p "$support_dir/bin" "$support_dir/state" "$support_dir/kakao-data" "$support_dir/kakao-ipc" "$support_dir/workspace"
cd "$repo_dir"
cargo build --release --locked
cp target/release/communication-hub "$support_dir/bin/communication-hub"
"$repo_dir/scripts/build-native.sh" "$support_dir/apps"
cp "$repo_dir/examples/contact-other.md" "$support_dir/contact-other.md"
# jq handles JSON escaping of arbitrary owner paths and model IDs.
jq -n --arg root "$support_dir" --arg home "$HOME" --arg model "$2" '{
    state:($root+"/state"),socket:($root+"/hub.sock"),
    app_server_socket:($home+"/.codex/app-server-control/app-server-control.sock"),
    contact_skill:($root+"/contact-other.md"),lookup_workdir:($root+"/workspace"),
    model:$model,effort:"medium",external_auto_send:false,dispatch_enabled:false,
    kakao:{enabled:false,account:"primary",legacy_state:($root+"/kakao-data"),
    receiver_app:($root+"/apps/Kakao Mention Receiver.app"),
    sender_app:($root+"/apps/Kakao Reply Sender.app"),sender_ipc:($root+"/kakao-ipc")}
}' > "$support_dir/config.json"
echo "Installed at $support_dir. No service started or macOS permission granted."
