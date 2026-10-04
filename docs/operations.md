# Operations

Runtime configuration and state are private, outside the repository. Never commit the generated configuration, SQLite databases, receipts, drafts, screenshots, or actual conversation evidence.

## Start and supervise

First verify foreground operation with the owner-approved configuration. On macOS, create a user LaunchAgent in `~/Library/LaunchAgents`. The important fields are:

```xml
<key>Label</key><string>org.communicationhub.service</string>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ThrottleInterval</key><integer>10</integer>
<key>ProgramArguments</key>
<array>
  <string>/ABSOLUTE/INSTALL/bin/communication-hub</string>
  <string>--config</string>
  <string>/ABSOLUTE/INSTALL/config.json</string>
  <string>run</string>
</array>
```

Replace placeholders with your actual paths and put stdout/stderr logs in a private state directory. Use `launchctl bootstrap gui/$(id -u) PATH_TO_PLIST` after validating your settings. Do not run the GUI adapter as a root LaunchDaemon. The receiver requires Full Disk Access; the sender requires Accessibility. With ad-hoc signing (the default), macOS binds those permissions to each build's code hash, so every rebuilt helper must be removed and re-added in System Settings and restarted. Sign with a stable local code-signing identity instead (`CODESIGN_IDENTITY="Your Identity" scripts/build-native.sh OUT`): the grant then follows the bundle identifier and certificate, and rebuilds keep it. A self-signed code-signing certificate in the login keychain works without being marked trusted.

## Inspect and pause

`status` reports dispatch/send gates, in-flight work, aggregate job states, sessions, and the last source heartbeat. Check the timestamp; an old `watching` label alone is not proof of health. `adapters` distinguishes supported operations from planned adapters.

`pause` durably disables new processing and subsequent sends. A model already running can finish preparing its result; a send already begun cannot be undone. `resume` processes pending intake. It does not blindly resend held or uncertain attempts.

Files under the configured state directory:

- `hub.sqlite3`: queue, scoped sessions/routes, introductions, and delivery journal.
- `control.json`: persistent pause gate.
- `source-kakao.json`: source status and observation time.
- `last-delivery.json`: recent result, including reply text; keep private.
- `daemon.lock`: process singleton lease.

Native IPC directories contain transient requests, receipts, draft recovery files, and staged bundles. These are sensitive runtime artifacts, not release assets. Requests are deleted after processing; receipts and recovery files currently require operator retention management.

## Recovery semantics

An interrupted `dispatching` event becomes `ambiguous`; an interrupted `sending` attempt becomes `sending_uncertain`. Inspect the real destination and receipt before deciding on a new attempt. A stale native phase file cannot prove that no UI input occurred, so timeouts are never labeled safely retryable from that file alone.

The receiver retries transport and temporary service/storage failures. Permanent policy rejection is acknowledged without replay. On restart, recently retained notifications can be submitted again, while event ID deduplication and age checks suppress duplicate or stale work. This cannot recover messages that never generated a usable notification or were removed from the retained notification database.

## Bundles

Create `attachment-bundles.json` in the configured Kakao data directory. Each conversation has its own approved bundle registry. Example with synthetic identifiers:

```json
{
  "rooms": {
    "EXAMPLE_CONVERSATION_ID": {
      "approved-export": {
        "json_source": "/ABSOLUTE/APPROVED/export.json",
        "result_zip": "/ABSOLUTE/APPROVED/results.zip",
        "filename": "project-export.zip",
        "description": "Operator-approved export for this conversation"
      }
    }
  }
}
```

`expected_issue_count` optionally checks an `issues` array. The older `taxonomy` field remains supported for migration and retains its legacy count check. No real project file or sharing grant ships with the repository. The content scanner is heuristic and does not replace a review of the approved source files.

## Upgrade and rollback

Build and test a new binary separately. Pause, wait for in-flight work, replace the installed binary atomically, and restart the user service. Confirm a new PID and a fresh heartbeat before resuming. Preserve the databases and pause state. Coordinate native helper changes with the config's IPC/state paths and macOS grants.

Optional legacy import reads old Kakao session, route, introduction, and event files from the configured Kakao data directory. Pending legacy work blocks import. Stop the old worker before cutover. Returning to an older implementation requires comparing both journals: completed/uncertain events are not synchronized back automatically. Do not delete history to force a replay.

## Board and call-log retention

The optional `dashboard` object controls the loopback port and `store_body` flag. No board is started when omitted. Use `board --open` rather than sharing its authentication fragment. The browser strips that fragment and keeps the read-only token in tab session storage; reopening the board is required after the service rotates it on restart.

`call_log` joins accepted/rejected calls to the durable event journal. It stores provider/account/conversation identity, message ID, observed tag, occurrence time, notification title, and optionally original body. The notification title is not authenticated sender identity. Do not export screenshots or DBs containing actual calls into the public repository. Body retention has no automatic expiration yet; set `store_body=false` before collecting sensitive workflows unless retention is intended. Previously stored bodies remain until the operator manages retention.

The board refreshes visible state every four seconds; backend observation runs on a bounded fifteen-second cycle and only reads metadata. Unavailable or stale probes are shown as unknown/offline, while `notLoaded` remains a stored-session state. Optional HTTP bind failure leaves the hub worker operational and records `port_unavailable`.
