# Locked macOS delivery

The daemon can prepare a reply while the Mac is awake, but a GUI adapter cannot promise delivery through the lock screen. Sleep, logout, Kakao logout and inaccessible GUI state are separate conditions. The hub does not wake a sleeping computer, unlock it, read credentials, type into loginwindow, disable OS protections, or inject into another process.

## Default: retain a final plan until unlock

`kakao.defer_locked_delivery` defaults to `true`. `deferred_delivery_ttl_seconds` defaults to 86400 and must be between 1 and 86400. A native `screen_locked` or `console_session_inactive` rejection is eligible only when its receipt explicitly confirms:

- `input_started=false`
- `side_effects_started=false`
- `text_sent=false`
- `attachment_sent=false`

Only the final reply is retained. ACK/busy notices are never replayed after unlock. The private SQLite `delivery_inputs` table retains the original Event beside the already-prepared Plan; no fresh model call occurs on resume. The journal records `lock_deferred`, a fixed total deadline and retry attempts. The management board labels this explicitly as 잠금·로그인 대기 and shows its total deadline, next check and attempts; it never labels a queued plan as delivered. Every 30 seconds, an eligible saved final plan may be leased again. Each attempt rechecks channel configuration, room approval, the called sister's enabled flag, room context reset, the original host-generated authorization stamp against current Contact-Other (and the called persona where applicable), this room’s project/bundle sharing definitions, total deadline, and the exact native room/trigger. Full name uniqueness checks are repeated; old chat-list hints are not reused.

If an already-lock-deferred plan encounters a stopped Kakao process or a clearly identified Kakao login window, a read-only readiness probe keeps that same plan in `waiting_for_kakao_login` until its original deadline. The probe reads role/subrole/title/description only; it never reads password or login-input AXValue. Missing or ambiguous AX metadata does not count as a proven login window. This exception does not apply to a first general offline request or an uncertain send.

Policy or room-scoped sharing changes also terminate an old prepared textual reply before any native probe or send: `Event.metadata.__hub_authorization_stamp` must exist and match the current host-computed stamp. After artifact staging or waiting for another UI operation, the adapter rechecks the shared delivery gate and host stamp immediately after acquiring its UI lock and again immediately before invoking the helper. Prewarming uses the same live room/sister/reset gate without treating a caller stamp as authority; it may only perform its existing target probe. A helper already invoked cannot be atomically undone by a later policy edit: native target/session checks still apply, but the owner must inspect an already-started or uncertain send. A legacy/missing/unknown stamp is never upgraded by assuming current permission. The stamp is internal host metadata, not caller authorization, and does not contain raw paths or policy text. Live Git/snapshot observations and unrelated valid project changes are excluded; the reply still describes its recorded observation time. Revocation, reset and expiration end the saved attempt. Success removes its Event snapshot. A sender timeout, uncertain UI write, ambiguous AX action, partial text/file result, or interrupted `sending` lease is never automatically retried. Restart recovery changes an interrupted send to `sending_uncertain`, so the daemon cannot duplicate it. This is a delivery outbox, not a model conversation session or cache flush.

ZIPs remain subject to the current approved artifact registry and validation when resumed. A file failure is not a successful delivery. Current project status answers should carry their observation time: a saved status reply reports the observation, not a claim that its contents are still current at eventual delivery.

## Experimental public AX text path

`kakao.locked_ax_text` defaults to `false`. It is an explicit opt-in experiment, not a claim that Kakao or macOS supports locked delivery. It applies only to **text-only** requests in an already-open, uniquely verified room with an empty composer. It uses public `AXUIElementSetAttributeValue` and the room's freshly bound, enabled send button's `AXPress` action. It does not activate or raise the app, alter focus, post keyboard/mouse events, access the clipboard, unlock the screen, or target loginwindow. Before and after writing it rechecks the same room, composer, exact trigger and observed session state. A new outgoing reply and empty composer are required for `sent_verified`.

If the preflight cannot establish those conditions, there is no external input and the plan is deferred. Once an input write has been attempted, any uncertain result is terminal for automatic retry. A successful `AXPress` return by itself is not proof of delivery. There is no verified public AX file-paste mechanism: a request with a ZIP/image is deferred in full before its text is written. Files are sent using the normal validated unlocked path after unlock.

The existing `CGSSessionScreenIsLocked` observation is not an Apple-documented lock-state contract. The sender also requires the documented console-session indicator and matching owner UID and fails closed when the session dictionary is unavailable/inactive. Each PID-scoped keyboard down/up event and every attachment clipboard/activation/focus/send boundary rechecks the unlocked session. A lock during upload preview ends the attempt; a lock after a file-send action starts leaves its result uncertain rather than inferring success from a disappearing sheet. Clipboard restoration requires the same unlocked owner session and unchanged pasteboard version; otherwise the receipt records a skipped restoration and does not overwrite another session or newer clipboard. Draft restoration is likewise withheld after a lock transition. Actual lock behavior must be checked on the target OS and Kakao build. `--watch-open` is read-only, suspends room observation outside the unlocked console session, and reports `session_locked`, `session_inactive` or `session_unknown` in its heartbeat; it never turns into a locked sender.

## Read-only probe and owner-controlled test

Use the **signed installed sender executable** that holds the existing Accessibility grant, not a newly compiled ad-hoc copy. The request below is synthetic. Replace the room and trigger only with the owner's self-chat and a unique test message already visible there. The probe does not create that message, open the room, activate Kakao, type, paste, or send anything.

```json
{
  "chat_name": "OWNER_SELF_CHAT_TITLE",
  "room_name_verified": true,
  "trigger_body": "UNIQUE_OWNER_TEST_TRIGGER",
  "reply": "[System-유이] : locked AX text test",
  "probe_locked": true,
  "expires_at": 9999999999
}
```

Pipe this private request to `SIGNED_SENDER_EXECUTABLE --probe-locked --ipc-dir PRIVATE_IPC_DIR`. The owner must lock/unlock using the normal OS interface. Probe output contains `session_state`, exact matching-window count, trigger-present Boolean, composer count, empty-draft Boolean, whether AXValue is settable, send-button count/action names, and `locked_text_candidate`. It omits message bodies and unrelated window titles. A candidate still requires the send-time complete chat-list uniqueness proof; it is not a successful-send result.

Only after the **locked** read-only probe reports the expected target and supported action, the owner can approve one text-only test by removing `probe_locked` and setting `locked_ax_text=true`. No attachment path is included. Verify the receipt and real outgoing self-chat after unlock. For a ZIP test, use a separate approved harmless archive: while locked the receipt must show zero input and `lock_deferred`; after normal unlock confirm exactly one text and one file, the same saved final Plan, and no new model invocation. Never retry an uncertain test blindly.

## Apple API evidence

- [AXUIElementPerformAction](https://developer.apple.com/documentation/applicationservices/1462091-axuielementperformaction): requests an action. Apple explicitly warns that `CannotComplete` need not mean the action failed; this adapter therefore does not retry it automatically.
- [AXUIElementCopyActionNames](https://developer.apple.com/documentation/applicationservices/1462053-axuielementcopyactionnames): reports the actions actually supported by an element; the adapter checks `AXPress` rather than assumes it.
- [AXUIElementSetAttributeValue](https://developer.apple.com/documentation/applicationservices/1460434-axuielementsetattributevalue): public attribute mutation; it provides no general locked-session delivery guarantee.
- [kCGSessionOnConsoleKey](https://developer.apple.com/documentation/coregraphics/kcgsessiononconsolekey): documented console-session indicator, not an authentication bypass or a file-send facility.

Real locked AX text/file delivery is **unverified** until the owner-controlled test succeeds. No credentials, private test bodies, self-chat titles, snapshots or receipts belong in the public repository.
