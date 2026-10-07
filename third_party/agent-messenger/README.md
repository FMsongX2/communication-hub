# Pinned KakaoTalk SDK subset

Source: https://github.com/agent-messenger/agent-messenger at
`f0a441dfb8f1865d54de26eaf3055de2df64a936`, version 2.39.0, MIT.

This is a 21-file SDK dependency subset, not a protocol reimplementation. CLI,
desktop credential extraction, persisted plaintext account managers, and unrelated
platform implementations are excluded. Explicit login helpers are retained for
owner onboarding; the daemon always receives credentials through macOS Keychain.

`UPSTREAM.json` records original and patched SHA-256 for every copied source file.
`communication-hub.patch` is the exact unified diff from that upstream commit.
`sidecar/kakao/verify-vendor.ts` checks the pinned commit and installed source hashes.

Local changes:

- All non-idempotent SDK operations use `executeOnce`; a dead-session exception
  cannot replay WRITE, SHIP/MSHIP, media upload, or LEAVE. Read operations retain
  upstream reconnect behavior. The sidecar calls only text and single attachment sends.
- `KakaoTalkClient` requires an explicit private sync-state directory. Calling
  `login()` without explicit credentials fails and cannot read/extract desktop credentials.
- Full/search `getChats` requests scan canonical LCHATLIST from zero cursors to
  explicit EOF instead of trusting the incremental LOGINLIST snapshot. Catalog
  membership comes only from that scan, excluding stale login entries. Missing
  pages/status/EOF/cursors, cursor cycles, duplicate-only non-EOF pages, and the
  50-page bound reject incomplete results. Default nonempty login views remain
  partial. Offline regressions are in `sidecar/kakao/catalog-completeness.test.ts`.
- `getLatestLogId` exposes the existing read-only CHATINFO watermark extraction for
  first-run cursor bootstrap. It does not enter a room or mark messages read.
- Two path aliases become relative imports; SHA-1 input copies into an ArrayBuffer
  view to match current TypeScript DOM crypto typings without unsafe assertions.

Install with `bun install --frozen-lockfile --ignore-scripts` here before installing
the sidecar package. Dependency versions and lockfile are pinned. No upstream npm
lifecycle script is run. Do not change source without updating the patch, manifest,
and no-replay regression tests.
