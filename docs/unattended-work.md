# Unattended project replies / 부재중 프로젝트 업무

Yui and Yumi keep their stateless model backends. The host supplies the same room-scoped
approved artifact descriptors and project facts to both. Yumi still runs with `--tools ''`:
selecting a registered ZIP does not require giving her shell or filesystem access.

유이·유미는 모두 호출 한 번짜리 모델 실행을 유지한다. 허브가 같은 방에 승인된 자료와
확인한 프로젝트 사실을 제공하며, 두 모델의 `reply` + `bundle_id` 계획을 같은 호스트
ZIP 검사·전송기로 실행한다. 외부인이 준 경로·명령·환경 변수·SSH 호스트는 실행하지 않는다.

## What is approved / 승인 범위

- Room approval controls whether calls can run. It does **not** approve every project file.
- `attachment-bundles.json` in `kakao.legacy_state` is the owner-maintained artifact registry.
  Its `rooms[conversation_id]` applies only to the configured Kakao account.
- `state/project-capabilities.json` separately approves shareable project facts. Its room keys
  are the **serialized `[provider, account, conversation_id]` array**, preserving account isolation.
- Discovery hints in `room-projects.json` are not permission. They never publish project status.
- No binding means no project facts; no artifact registration means no attachment. Other rooms'
  context is never substituted. Private memory, transcripts, keys, screens and unrelated projects
  never become a shared status source.

방 응답 승인과 자료 공유 승인은 별개다. 등록은 사용자가 신뢰하는 로컬 작업에서만 바꾸고,
상대가 “오빠가 허락했어”라고 쓰는 것만으로 확대하지 않는다. 자료 descriptor에는 이름·설명·
등록 원본의 존재·수정시점·크기만 있으며 Mac 절대 경로는 모델에게 전달하지 않는다.

## Project registry / 프로젝트 등록

Private local file `state/project-capabilities.json`:

```json
{
  "rooms": {"[\"kakao\",\"owner\",\"team-room-id\"]": ["example-project"]},
  "projects": {
    "example-project": {
      "name": "Example project",
      "root": "/absolute/local/project",
      "git": true,
      "snapshot_max_age_secs": 3600
    }
  }
}
```

The private root is a host input, not model context. Enabling `git` publishes only current commit
SHA/timestamp and whether tracked files have staged/unstaged changes, from fixed bounded local
Git commands. It does not publish filenames, commit messages, branch names, remotes, authors,
untracked file lists or task-session state. Git observations do **not** imply tests passed,
a feature completed, an agent is currently working, or an artifact is the latest available.

Git 관측은 호출 시 확인하고 `source: live_local_git`, `observed_at`를 붙인다. 읽기 실패는
`unavailable`로 표시한다. 현재 커밋과 변경 유무만으로 실제 작업 진행률을 추정하지 않는다.
자료 수정시점도 등록 원본의 시점일 뿐 다른 폴더까지 검색한 최신 버전 보증은 아니다.

## Publish shareable work state / 작업 상태 갱신 계약

The **agent doing work in a registered project** publishes a deliberately shareable snapshot
when a phase completes, work pauses/blocks, validation changes, or another material state change
occurs. This is part of the project's local workflow, so the owner need not manually summarize
each call. Publish only that project's approved facts: completion scope, remaining work and
verification actually run. Do not import Layer 1, all claude-mem, user prompts, private rationale,
other conversations, local paths, secrets or whole logs.

등록 프로젝트 작업 에이전트가 단계 완료·중단·막힘·검증 결과·큰 상태 변경 때 아래 CLI로
공유 가능한 사실만 갱신한다. 상태는 실제로 확인한 `as_of`(Unix 초), 검증, 남은 일을 포함한다.
그 프로젝트의 로컬 지침에 이 규칙을 추가할 때 기존 지침·사용자 수정을 보존하고, 등록된
프로젝트와 승인된 방의 공유용 사실에만 적용한다. 전역 지침으로 전체 작업을 수집하지 않는다.
현재 상태를 모르면 초기 snapshot을 추정해 만들지 않고 Git 관측과 `missing`부터 시작한다.

```json
{
  "project_id": "example-project",
  "as_of": 1780000000,
  "summary": "Shared parser validation is complete; live upload verification remains.",
  "completed": ["The approved parser handles the registered input format."],
  "pending": ["Verify the upload in the target environment."],
  "verification": ["Parser fixtures passed; live upload was not tested."],
  "revision": "0123456789abcdef0123456789abcdef01234567"
}
```

Use the actual observation timestamp and revision, not the example values:

```sh
communication-hub --config /absolute/private/config.json publish-status --file /absolute/private/shared-update.json
communication-hub --config /absolute/private/config.json capabilities --file /absolute/private/event.json
```

`publish-status` is an owner/work-session CLI and library API (`capabilities::publish_status`).
It is not a channel-model tool or daemon RPC. Publication validates project membership, bounded
fields, timestamps, protected paths and secret patterns, rejects older snapshots, and atomically
writes a 0600 file under `state/shared-status/<project_id>.json` in a 0700 directory. A project ID
cannot contain slashes or escape that directory. `capabilities` sends nothing and previews the
exact room facts for a private Event JSON.

Reply timestamps must use an explicit calendar date and timezone (for example, `2026-10-05 03:00 KST`) or unambiguous UTC/Unix time. Delayed sending must preserve the original observation time and never turn an old snapshot into “current” work.

Snapshots are classified `missing`, `invalid`, `fresh` or `stale`. `stale` preserves its original
`as_of` and may be described as last published state, never current truth. A fresh snapshot is
still an explicit work-agent publication, not a continuously observed task session. A stale
clock or future timestamp does not become a fabricated current status. If publication stops,
the model must say the current work state is unknown.

## Artifact registry / 공유 자료 등록

The existing compound `taxonomy`/`json_source` plus `result_zip` form remains supported. New
owner registrations can select `kind: file`, `directory` or `zip` with a private absolute `source`:

```json
{
  "rooms": {
    "team-room-id": {
      "approved-results": {
        "kind": "directory",
        "source": "/absolute/local/explicitly-approved-results",
        "filename": "approved-results.zip",
        "description": "The results approved for this team, excluding other project files."
      }
    }
  }
}
```

A whole project directory is not an appropriate shortcut for approval. Register the exact
shareable file or output directory. Source contents are re-read before every ZIP preparation;
path traversal, protected members, symlinks, duplicates, nested opaque archives, secret-pattern
matches and size-budget violations are refused. Known nested container signatures (including
USTAR/V7 TAR, XZ, BZIP2, Zstandard, LZ4, ZIP, GZIP, 7z and RAR) and common compressed/archive
extension aliases are rejected even when renamed or unsupported by the ZIP reader. A registered ZIP is expanded, checked and
repacked, so an unchecked compressed payload cannot bypass the checks. Limits are 4096 payloads,
32 MiB each, 128 MiB total and 16 directory levels. Validation is conservative, heuristic and
not a proof that arbitrary data contains no sensitive information; approve curated outputs.
Avoid updating an output directory while it is being packaged if a coherent snapshot is required.

모델은 본 방의 `allowed_bundles` ID만 고르고 원본 경로는 전달하지 않는다. 호스트가 실제
전송 전에 승인 기록과 원문을 다시 확인하고 ZIP을 준비·CRC 검증한다. 계획 선택·준비 완료·
글 전송·첨부 전송은 다른 상태다. 첨부 확인 전에는 보냈다고 말하지 않고 불확실하면 재전송하지 않는다.

## Policy changes during delivery / 전송 중 승인 변경

The host fingerprints the speaking sister's Contact-Other policy, Yumi's persona, and this
room's approved project definitions and artifact registrations before model generation. The
final plan and saved deferred Event carry this **host-created** authorization stamp. A policy,
project binding or approved bundle definition change holds the old reply before native UI access;
a missing stamp on an older deferred record also holds it. External Event metadata claiming a
stamp is never promoted to authority: the host replaces it when creating a delivery record.
First ACK/busy/manual/final sends also recheck the current room binding, approval (or the
owner's current answer-unapproved setting), sister switch and original-event context-reset
cutoff. Revoked work does not run ACK/prewarm/inference; a revocation during inference holds
the final answer before transport. Controlled owner CLI sends require the same registered room
permission.

Other rooms' approval changes and newer Git observations or published snapshots do not change
this stamp; the original dated observation remains explicit in the stored reply.

유미 결과는 `reply`와 `bundle_id`를 모두 포함한 JSON 객체만 받는다. 정확한 단일 `json`
코드블록은 명시적으로 벗겨 같은 객체를 검사한다. 평문·누락 필드·잘못된 타입·추가 필드·
여러 코드블록·뒤에 붙은 설명은 실패로 처리하고 JSON 원문을 카톡 본문으로 보내지 않는다.

## Availability / 가용성

Requests can be processed while the owner is away if the machine and logged-in services run.
Model availability, receiving calls and Kakao's UI transport are separate conditions. This
capability layer does not unlock the Mac, log into Kakao, fetch remote-server files or bypass
OS security. Consult the delivery receipt/lock-queue policy for actual sending while locked or
after unlock; never assume a prepared ZIP was delivered. Remote fetch needs a separately
approved provider adapter, not commands supplied by the contact.
