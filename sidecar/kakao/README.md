# Persistent Kakao LOCO sidecar

A single client owns both receive and send. It uses the pinned upstream Kakao SDK
in `../../third_party/agent-messenger`; no CLI-per-message process or UI transport
fallback exists here. Bun 1.4.2 was used for offline validation.

```sh
bun install --cwd third_party/agent-messenger --frozen-lockfile --ignore-scripts
bun install --cwd sidecar/kakao --frozen-lockfile --ignore-scripts
bun run --cwd sidecar/kakao verify-vendor
bun run --cwd sidecar/kakao typecheck
bun test --cwd sidecar/kakao
bun sidecar/kakao/main.ts --config /absolute/private/config.json
```

`ConfigSchema` in `types.ts` is the source of truth. Required fields:
`socket_path`, `hub_socket`, `state_dir`, `expected_user_id`, `credential_service`,
`credential_account`, `allowed_chat_ids`, `mode` (`shadow` or `active`),
`attachment_roots`. Paths are absolute. State and socket directories must be owned
by the current user with mode 0700; the Unix socket and SQLite database are 0600.
No credentials are accepted from the environment, config, or request. The injected
credential/client providers in `runtime.ts` are for offline tests; the CLI always
uses the Keychain reader and pinned SDK.

The account lock has a fixed path independent of configured state directories:
`~/.local/state/communication-hub/kakao-locks/<user_id>.lock`. A second process fails
closed. After SIGKILL, an operator must verify the recorded PID/process is gone and
remove the stale lock and socket before restart; automatic PID-based stealing is
intentionally avoided. Keep the SQLite database on recovery. Normal SIGINT/SIGTERM
closes client/socket and releases the lock. Runtime must remain awake and connected;
actual screen-lock or tablet/desktop coexistence has not been tested.

## IPC and delivery

One JSONL request per connection, maximum 128 KiB, up to 32 simultaneous connections.
Requests: `{id:string,method:'status'|'list_rooms'|'list_chats'|'resolve_chat'|'refresh_policy'|'room_info'|'message_page'|'send',params:{...}}`.
Responses: `{id,ok:true,result}` or `{id,ok:false,error:{code}}`.
`send` takes `delivery_id`, `expected_user_id`, `chat_id`, `reply`, Unix-seconds
`expires_at`, and optional `attachment_path`/`attachment_sha256` (null accepted).
The complete normalized payload, **including the original expiry**, must stay
identical on retries. Any changed payload under the same delivery ID is held.

The durable journal is written with SQLite FULL synchronous WAL before SDK dispatch.
No duplicate delivery ID ever dispatches again, including after a crash. A valid
server ACK requires success, matching chat, positive `log_id`, and positive `sent_at`.
Text and attachment receipts are saved independently. `sent_verified` means server
acceptance, never recipient read. Missing/malformed ACK or a thrown dispatch stays
`sending_uncertain`; no automatic fallback/retry. If text succeeded but attachment
has not been dispatched because readiness or expiry changed, `partial_file_held`
preserves the text receipt. A thrown attachment dispatch stays uncertain.

Attachment content must match its SHA-256, be a nonempty regular file under an
approved resolved root, and be at most 20 MiB. Bytes are read before text dispatch.
Shadow mode cannot send. Account identity, room allowlist, expiry and readiness are
validated before dispatch. Immediately before each text/attachment component, the
sidecar calls `{method:"authorize_kakao_loco",params:{delivery_id,user_id,chat_id,component}}`
and requires `{ok:true,result:{status:"authorized"}}`. Hub rechecks its persisted
sending lease, event/host stamp, current room/policy/pause and bundle authorization.
An unavailable/denying authorizer cannot dispatch; after a text receipt it produces
`partial_file_held`. Room mapping/permission ownership remains in Rust Hub.

## Receive and recovery

Only allowlisted chat IDs wake the receiver. Pushes trigger ordered forward
`getMessagePage` calls; push arrival order never advances the cursor directly.
On the first run per room, read-only CHATINFO establishes a watermark so existing
history is not flooded into Hub. A push observed while that snapshot is in flight
lowers the bootstrap to its predecessor so the new message remains eligible.

Every accepted event carries sidecar `mode`, user/chat/log/author IDs, body, time,
and title. The cursor advances only after durable Hub ACK `queued`, `duplicate`,
`ignored`, `shadow`, or `rejected` (all entries for a multi-agent ACK). Own known
outgoing receipt IDs and self-authored `[System-유이] : ` / `[System-유미] : ` echoes
are locally excluded. Ordinary owner-authored tagged messages are preserved.

Catch-up is bounded to 10 pages of at most 100 messages per pass. Hitting the cap
sets `catchup_limit`, keeps the last committed cursor, and holds sends. This is an
explicit incomplete state; it must not be presented as complete history recovery.
After review an operator may restart to process the next bounded segment; never
reset/delete the cursor just to clear the hold. Protocol retention/history gaps
cannot be reconstructed by this sidecar and need live validation.

Ordinary disconnects use 1–30 second bounded exponential backoff and the same client;
Hub failure retries from the durable cursor. KICKOUT is terminal and cannot reclaim
the account automatically. Invalid/expired token becomes `auth_required`, requiring
explicit owner reauthentication. No refresh-token extraction or automatic refresh
is implemented. Remote history anti-entropy runs every 30 seconds by default;
normal pushes request reconciliation after 50 ms and query only dirty approved rooms.

## Dynamic room policy and GUI catalog

Set optional `dynamic_room_policy: true` only with a Hub that implements the
contracts below. The default remains static-config compatibility. In dynamic mode,
`allowed_chat_ids` is never a fallback: the Hub's durable SQLite approvals are the
only room authority. The original config IDs are retained solely for the bounded
one-time cursor adoption described below.

The sidecar pulls `{method:"loco_room_policy",params:{user_id}}` at startup, on
`refresh_policy`, before diagnostic room reads, and every five seconds over the
local Unix socket. This is a local policy request, not a remote room/catalog scan.
The Hub response is `{ok:true,result:{status:"ready",user_id,mode,revision,rooms}}`;
`revision` is a string and each approved room is
`{room_id,chat_id,name,approval_epoch}` with a durable string `approval_epoch`.
User ID and mode must exactly match the sidecar; duplicate room/chat IDs, malformed
snapshots, and unreachable Hub fail closed. A temporary outage clears active
permissions and cancels pending room work without erasing durable cursors. A later
valid policy restores access; revoked config rooms cannot be resurrected.

`refresh_policy` accepts only empty params and returns
`{status:"ready",user_id,revision,rooms:[{chat_id,status,reason?}]}`. Its top-level
status means the policy was applied; per-room status distinguishes `sync_pending`,
`ready`, and `held`. Adding a room starts a read-only watermark/bootstrap without
restarting the SDK session. Removing a room clears dirty/pending/issue state and
cancels its sync waiter. After every asynchronous page, watermark, or Hub ACK,
the receiver verifies the current runtime scope before ingestion or cursor advance.
A failure in one room does not prevent other approved rooms from being ready.

`approval_epoch` changes when the Hub deletes/re-adds or changes a binding. The
sidecar persists each epoch beside its cursor; an epoch change discards that receive
cursor and starts at a new watermark. Calls from the unapproved interval and calls
at/before the initial dynamic watermark are not replayed. An unchanged epoch across
restart preserves the cursor and backfills approved downtime. The first valid
policy migration may adopt an existing static cursor only when there is no persisted
dynamic-initialization marker, the room was in the original static allowlist, the
current Hub snapshot approves it, and its epoch is `"1"`. The marker is durable and
is not set on policy failure. All other newly approved or changed rooms bootstrap.
Completed/uncertain delivery and outgoing-receipt journals are never erased.

Dynamic `send` requests must include the current `approval_epoch`. The sender also
captures a runtime scope before entering its queue and rechecks it after session
and host-authorization awaits, and between text and attachment components. Ingest
and `authorize_kakao_loco` payloads include `approval_epoch`; the Hub must validate
it against durable current policy before dispatch so an already in-flight IPC
request cannot cross a revocation/re-add boundary. A stale queued request is held.

`list_rooms` and `list_chats` are aliases returning
`{status:"ready",user_id,rooms:[{chat_id,name,type,member_count}]}` from a fresh,
complete `getChats({all:true,resolveTitles:true})` catalog: all LCHATLIST pages
plus missing candidates from current-session LOGINLIST and the materialized sync
snapshot negotiated at login, verified through fresh CHATINFO and stable GETMEM
membership. Config IDs and unvalidated persisted records are never added. Duplicate display
names remain distinct by chat ID. These responses include no messages, previews,
last-message fields, or member profile data. `resolve_chat({chat_id})` rescans that
same authenticated account catalog and returns only the selected room's metadata;
unknown/stale IDs fail. Catalog operations never enter rooms, send messages, grant
access, or invoke models. Existing `room_info`/`message_page` diagnostics still
require current approval and discard results if revoked while awaiting the SDK.

## Validation boundary

Offline tests inject fake credentials and fake transport; the no-replay tests call
actual patched upstream SDK methods. No real login, desktop cache extraction,
message send, device registration, launch-agent deployment, or live latency claim
is part of these tests. Follow the owner onboarding/pilot runbook before enabling
live receive or send.

`catalog_diagnostics` (private IPC only, empty params) returns metadata-only
source chat-ID sets from the most recent successful full scan: current LOGINLIST,
negotiated materialized sync IDs, LCHATLIST continuation, separately validated/
excluded login-only and sync-only candidates, tombstones, and
returned count. It performs no network calls and contains no raw packet bodies,
message content, or credentials. Failed full scans clear this diagnostic snapshot.
