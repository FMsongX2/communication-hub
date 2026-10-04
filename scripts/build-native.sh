#!/usr/bin/env bash
set -euo pipefail
repo_dir="$(cd "$(dirname "$0")/.." && pwd)"
output_dir="${1:?usage: build-native.sh OUTPUT_DIRECTORY}"
# Ad-hoc signing ("-") ties macOS permissions to each build's code hash, so every rebuild
# needs Accessibility/Full Disk Access granted again. A stable local code-signing identity
# keeps the grants across rebuilds.
sign_identity="${CODESIGN_IDENTITY:--}"
mkdir -p "$output_dir"
build_app() {
    local app_name="$1" executable_name="$2" bundle_id="$3" source_name="$4"
    local app_dir="$output_dir/$app_name.app"
    mkdir -p "$app_dir/Contents/MacOS"
    cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>$executable_name</string>
<key>CFBundleIdentifier</key><string>$bundle_id</string>
<key>CFBundleName</key><string>$app_name</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>LSUIElement</key><true/>
</dict></plist>
PLIST
    if [[ "$executable_name" == "KakaoMentionReceiver" ]]; then
        swiftc -O "$repo_dir/native/kakao/$source_name" -o "$app_dir/Contents/MacOS/$executable_name" -framework AppKit -lsqlite3
    else
        swiftc -O "$repo_dir/native/kakao/$source_name" -o "$app_dir/Contents/MacOS/$executable_name" -framework AppKit -framework ApplicationServices
    fi
    codesign --force --sign "$sign_identity" "$app_dir"
}
build_app 'Kakao Mention Receiver' KakaoMentionReceiver org.communicationhub.kakao.receiver NotificationReceiver.swift
build_app 'Kakao Reply Sender' KakaoReplySender org.communicationhub.kakao.sender ReplySender.swift
