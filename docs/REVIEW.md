# Release review

The alpha release was reviewed using source inspection, contract/failure tests, local native compilation, a previously exercised live notification-to-reply flow, and RustSec dependency audit. It is not a claim of universal compatibility or a formal security audit.

| Finding | Resolution / verification |
| --- | --- |
| Timeout phase could be stale after UI input | Keep timeout uncertain; remove phase-based termination/safe-retry inference. Native requests expire before side effects. |
| Temporary storage failure looked like permanent rejection | Distinct retryable error response; native receiver reads/parses a complete response. Contract test holds the SQLite write lock and retries. |
| Shared temporary filename raced concurrent writes | Random exclusive temp files, file and parent-directory sync. Concurrent-write test verifies valid private JSON. |
| Accessible conversation name could be ambiguous | Recheck the full exposed AX row set and unknown-room preview; fail closed on incomplete or duplicate targets. |
| Restart could overlap native senders | Native sender holds a per-IPC UI lease. No automatic replay of uncertain sends. |
| Public configuration assumed a particular machine/model | Generic install paths, explicit supported model selection, configurable IPC/state locations, disabled automation defaults. |
| Native observed-ID set could grow forever | Bound it by retaining currently observed IDs after the cache threshold. |
| Archive metadata size could understate expanded bytes | Bound actual decompression per entry and cumulatively, in addition to declared sizes. |
| Private runtime material mixed with publishable source | Fresh Git history and explicit source allowlist; local/runtime/build artifacts ignored. Synthetic example policy only. |

Validation: 19 Rust contract tests, formatting and Clippy, native builds and a read-only native target/duplicate-name verification (no message sent), and cargo-audit with no reported vulnerabilities or warnings for the locked dependencies at review time. CI repeats the build/test checks. Audit results are time-dependent.

An optional independent Opus review could not launch because this directory was not an Orca-managed worktree. No independent model endorsement is claimed.

Remaining limitations: only KakaoTalk implemented; native attachment end-to-end validation pending; notification absence/truncation; UI localization and virtualized rows; configured account aliases rather than authenticated account detection; same-UID local trust; heuristic content checks; no automatic retention cleanup; experimental backend protocol; Accessibility queries can be slow on large histories. These are documented in both READMEs.

### Optional expression delivery and wire prefix

The Rust runner now carries an optional sticker ID from bounded operator-owned catalog candidates to a two-phase native delivery. Tests cover catalog path/hash rejection, original PNG/GIF bytes, unverified GIF gating, image-hash deduplication, room-scoped attempt cooldown, symlink staging rejection, attachment exclusivity, and code-generated prefix normalization. The 34 Rust tests and warning-free Clippy passed locally; the Swift sender compiled with AppKit/ApplicationServices.

A controlled self-conversation on the operator Mac received its text and PNG in order. That Kakao version exposed no filename in the image bubble, so the initial native receipt correctly remained uncertain rather than claiming success; no automatic resend occurred. Completion detection now supports a filename-verified preview followed by a newly observed outgoing image bubble. GIF delivery/animation verification is separate, and is disabled by default. No live conversation data or third-party sticker assets are included in the repository.

The follow-up controlled GIF attachment was reported `sent_verified` by the native sender. The operator Mac displayed a GIF badge and distinct animated frames in Kakao’s image viewer. Text was not resent: after login interruption, the owner-authorized check inspected the conversation and continued only the held attachment. GIF auto-selection was enabled on that tested private installation; fresh installs remain disabled by default.
