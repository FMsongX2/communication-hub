# Kakao LOCO transport pilot

This transport replaces the Kakao UI adapter behind the existing Hub policy, queue,
conversation approvals, and delivery journal. The AX implementation remains the
explicit rollback choice. A single persistent Bun sidecar owns the tablet session,
including both listener and outbound requests. Never launch a competing Kakao CLI
listener, sidecar, or login process for that account.

## Pinned source and preparation

The vendored Kakao SDK is agent-messenger 2.39.0, commit
`f0a441dfb8f1865d54de26eaf3055de2df64a936`, from
<https://github.com/agent-messenger/agent-messenger>. It is MIT licensed; the license,
upstream hashes, and local diff are retained in `third_party/agent-messenger`.
The local patch disables ambiguous outbound write replay and implicit desktop
credential extraction. Use the verifier and both frozen dependency lockfiles when
rebuilding. Do not update the pin without repeating fault-injection tests.

```sh
./scripts/kakao-loco-prepare.sh
python3 scripts/kakao-loco-readiness.py
```

Preparation installs pinned dependencies without lifecycle scripts, verifies source
hashes, runs offline tests and type checking, compiles the Keychain helper, and
writes an unloaded LaunchAgent candidate under `local/kakao-loco-migration`.
It does not access Keychain, authenticate, send messages, copy launch agents, or
change the installed Hub configuration. The native helper self-test has no
Keychain side effects. The candidate has `RunAtLoad=false` and `KeepAlive=false`;
connection retry belongs to the sidecar, while terminal account errors must stay
visible instead of being hidden by a service restart loop.

The listener coalesces push notifications by approved chat ID and catches up only
those dirty rooms. Pushes arriving during a catch-up remain queued for the next
pass. Initial connection and reconnection refresh room metadata and catch up all
approved rooms; ordinary pushes do not list every room again. A separate
anti-entropy sweep catches up all approved rooms every 30 seconds by default,
without refreshing room metadata. Set `KAKAO_LOCO_ANTI_ENTROPY_MS` to an integer
between `30000` and `3600000` to change that interval. Explicit `list_rooms` also
refreshes room metadata. SIGINT/SIGTERM cleanup is installed before initial
credential/session awaits, so interrupting startup releases the account lock and
does not start a client after the pending credentials arrive.

## Owner authentication

The one owner action is to run this command **in their own terminal** and complete
the phone approval while prompted:

```sh
./scripts/kakao-loco-onboard.sh
```

It reads the existing config from
`~/Library/Application Support/CommunicationHub/config.json`. Optional positional
arguments are an existing Hub config path and a new candidate output directory;
they are paths only. Never put an email, password, token, or phone code in command
arguments, chat, redirected input, recordings, or log files.

The owner first types `CONNECT`, then enters account and password with terminal
echo disabled. Only the verified SDK `loginFlow` helper is used. It receives
explicit inputs, `deviceType: tablet`, and `force: false`; upstream `auth login`
and desktop `Cache.db` extraction are never invoked. If Kakao requires registration,
the code is displayed directly on the owner's controlling terminal for approval
on the phone. An occupied tablet slot or failed approval stops the flow. There is
no automatic force-login or PC-slot switch. A normally authenticated previously
registered device need not receive another phone challenge; the client does not
bypass any challenge required by Kakao.

The resulting `{oauthToken,userId,deviceUuid,deviceType:'tablet'}` JSON is saved in
macOS Keychain under service `dev.communication-hub.kakao-loco`, account
`owner-tablet`. The native Security-framework helper receives it on stdin and
returns it only through a private child-process pipe. It does not use the
`security` CLI, shell interpolation, credential environment variables, or
plaintext SDK credential files. Existing Keychain entries are never overwritten
by onboarding. SDK refresh tokens and the password are not persisted. Javascript
strings cannot guarantee cryptographic memory erasure; they are cleared from
local variables after the login attempt.

The helper reads with Keychain interaction disabled. If Keychain is locked or
access is refused, stop and resolve that in the owner's logged-in session; do not
try to unlock macOS or the Keychain automatically. After helper replacement, verify
access in the owner's foreground session before unattended operation. Authentication
expiry requires a deliberate owner re-authentication procedure, not an implicit
desktop-cache refresh. Existing credentials must be explicitly retired by the
owner before re-onboarding.

Successful onboarding writes only:

- `~/.communication-hub/kakao-loco/sidecar.shadow.json`, with empty approved chat IDs.
- `~/.communication-hub/kakao-loco/hub.shadow.candidate.json`, with empty room mapping,
  dispatch disabled, and external auto-send disabled.

These files are private (`0600`) in a private directory. The Hub candidate preserves
the actual source paths, including installations whose config and socket directories
differ. It is **not applied automatically**. If the Keychain insert succeeded but
candidate creation failed, `bun sidecar/kakao/onboarding.ts stage` regenerates
candidates in a new output directory without another account login. Do not start
a second Hub process against the same live database.

## Account and room binding

After checking the candidate account ID and completing the offline preparation,
start one sidecar in the owner's foreground terminal:

```sh
bun sidecar/kakao/main.ts --config "$HOME/.communication-hub/kakao-loco/sidecar.shadow.json"
```

Use a second local terminal for read-only status and room metadata:

```sh
python3 scripts/kakao-loco-rpc.py status
python3 scripts/kakao-loco-rpc.py list_rooms
```

Review results locally; they can contain private room names and account IDs. Bind
existing approved Hub conversation IDs to the **verified numeric Kakao chat IDs**
in candidate `kakao.loco.rooms`. Populate the sidecar `allowed_chat_ids` with exactly
those IDs, and ensure both `expected_user_id` values match the authenticated owner.
Names alone are insufficient approval evidence, especially duplicate room names.
Start with the account's verified self-chat. Leave all other rooms unapproved
until their identity and current Hub approval are reconciled. Keep both transport
modes `shadow` while comparing reception. The sidecar's initial room cursor must
not retroactively dispatch old chat history.

## Rollout gates

1. **Read-only shadow:** compare incoming IDs, author IDs, timestamps, deduplication,
   and disconnect/reconnect catch-up against the existing receiver. No model
   execution or outgoing traffic from the shadow feed. Persist before cursor
   advancement; inspect any history gap instead of silently skipping it.
2. **Owner self-chat:** pause live dispatch and wait for in-flight deliveries, back
   up the current config/database, then apply the reviewed candidate and enable
   only the self-chat route. Use one uniquely tagged text and synthetic PNG, PDF,
   and ZIP. No real third-party messages or personal attachments in this pilot.
   Register the explicit attachment roots/approved Hub bundles before testing.
3. **Receipt and ambiguity:** a successful send needs a valid positive `log_id`
   plus a matching account and chat. A server receipt is not a recipient-read
   receipt. Treat timeout/disconnect after dispatch as `uncertain`; never replay
   automatically, including by switching to AX. Reconcile against the room log
   and delivery journal before a new explicit send.
4. **Restart and lifecycle:** test incoming duplicates, restart after persistence,
   catch-up, ordinary network recovery, terminal kickout, and no two owners of
   the same session. Test screen-locked delivery only with the Mac awake and
   network available. Screen lock and system sleep are different conditions;
   do not claim unattended sleep delivery.
5. **Measured promotion:** record detection and transport P50/P95 separately from
   model latency, then enable one approved room at a time. Install/load the reviewed
   LaunchAgent only after foreground and account-coexistence checks pass. Production
   auto-start policy is an explicit deployment decision; the staged plist does not
   boot automatically.

The JSONL Unix socket accepts `{id,method:'status'|'list_rooms'|'send',params:{...}}`
and responds with `{id,ok,result}` or `{id,ok:false,error}`. Only the Hub applies
conversation policy; the sidecar adds an account/room allowlist and filesystem
attachment boundary. Sidecar logs, event databases, cursor state, and receipt
evidence remain private and outside version control.

## Rollback

Pause dispatch, settle in-flight requests, stop the sidecar, preserve all journals,
restore the backed-up Hub configuration without `kakao.loco`, and restart the
original Hub/AX receiver. Confirm only one receiver is dispatching before resuming.
Never erase or reset completed/uncertain delivery entries to force a resend, and
never automatically move an ambiguous LOCO delivery to AX. The rollback does not
log out or delete credentials on the owner's behalf.

## Current verification boundary

Offline auth and Keychain IPC contract tests verify explicit inputs, disabled force
login, rejected missing credentials, non-overwrite behavior, and private shadow
candidate generation. The Swift helper compiles and its self-test does not access
Keychain. No account authentication, live protocol messages, phone approvals,
Keychain insertion, deployed service, latency comparison, or locked-screen send is
implied by those tests. Record live evidence only after the owner login and rollout
gates actually complete.
