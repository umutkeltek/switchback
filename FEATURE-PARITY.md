# Reference-project coverage

Reviewed 2026-11-10. This is a coverage map, not a claim of full parity.
Switchback keeps routing, account configuration, settings transactions, and
capture verification in Rust. Native process helpers currently also use Python
and zsh. A capability counts as finished only after its real caller is verified.

References: [CC Switch](https://github.com/farion1231/cc-switch), MIT,
README blob `7b9ff6803686b5393ab8805955ec064b04102d82`; and
[CodexBar](https://github.com/steipete/CodexBar), MIT,
README blob `a8dec074538299d7fa40d55856a1670f699860c8`.
Those snapshots are dated evidence; upstream descriptions do not prove behavior.

| Capability | Switchback coverage | Remaining acceptance work |
| --- | --- | --- |
| Isolated Claude/Codex accounts | Native homes, identity binding, login flows, ordered pools | More native subscription providers need their own vendor-supported adapters |
| Interactive account selection | Claude picker, explicit account override, saved default | Equivalent picker and convenience flow for other native clients |
| Credential renewal | Vendor-owned renewal, retained failure state, explicit login recovery | Interactive consent cannot be completed unattended merely by retaining a token |
| Multi-account continuation | Explicit Claude transcript forks; Codex same-account continuation | Cross-provider continuation requires a separate validated task/checkpoint contract |
| Usage and reset windows | Native Claude/Codex sources; configured CodexBar collectors; freshness and identity checks | Independent Rust collectors for the remaining supported upstream providers |
| Pool availability | Rust read-only pool report, per-account failures and resets | Compound must bind and consume this through its existing admission owner |
| Away execution | Capture-required adapter and explicit posture checks | Pool capacity, settings/hook bindings, and live execution inside an authorized away window |
| Full request/response capture | Segmented storage, exact attempt correlation and hashes, bounded terminal-stream drain | Readable, sealed, and backed-up evidence remain separate |
| Protocol conversion and provider routing | Canonical Rust IR, codecs, ordered fallback, circuit breakers, account locks | Compare provider-specific rectifiers and failure fixtures individually |
| Provider presets and setup | Rust provider presets, configuration/setup commands, launch profiles | Preset-by-preset import, validation, and setup coverage across CC Switch's tool roster |
| Safe configuration updates | Revision checks, owned artifacts, journal recovery, shared-settings merge | Field-preserving editors/importers for every supported third-party client |
| Project snapshots and switching | Launch profiles and per-profile configuration | User-facing project snapshot lifecycle and switching workflow |
| MCP configuration | Existing MCP CLI and profile declarations | Unified discovery, import/export, validation, and editor parity across clients |
| Skills and prompts | Harness/profile integration exists | Complete cross-client discovery, installation, editing, and synchronization |
| Sessions | Native transcripts, capture records, metadata traces | Unified search, browser, resume/export, workspace and memory editors |
| Usage/cost accounting | Rust usage ledger, provider pricing, trace attribution | Subscription-aware history charts and account/model/project/session breakdowns |
| Historical local usage | Selected CodexBar history imports | Bounded SQLite usage history, retention, repair, and full local-log scanning |
| Adaptive refresh | Explicit bounded refresh commands | Coalesced scheduler, transport-error backoff, reset refresh, activity/energy policy |
| Status and notifications | CLI diagnostics | Provider incident feeds, reset/limit notifications, widgets |
| Desktop/tray experience | Local dashboard and CLI | Full tray/menu UI, native widgets, accessibility and localization parity |
| Sync and portability | Local owned configuration and backups | Encrypted/conflict-safe WebDAV/S3 sync, import/export, deep links |
| Installation and updates | Owned installers, vendor pins, doctor/repair commands | Cross-platform updater and duplicate-install diagnosis for all client tools |

## Architectural choices

- Reuse the canonical IR and composed adapters. Do not introduce a separate
  converter for every pair of provider protocols.
- Keep account identity, quota evidence, routing, permission, and task completion
  separate. A successful request is not proof of durable capture or spare quota.
- Follow CC Switch's native CLI refresh ownership and account-bound usage cache.
  Its `claude_account/usage.rs` also serializes/throttles probes; Switchback still
  needs a unified refresh scheduler before claiming that behavior across providers.
- Follow CodexBar's timestamp-preserving transient-error handling and explicit
  source identity. Authentication failures and changed identities must invalidate
  availability even when historical readings remain visible.
- Keep native account files isolated. Copying global credentials or sharing
  transcript directories would weaken both identity and continuation guarantees.
- Add convenience features through the existing owners. Provider-specific
  permission shortcuts must not widen an agent's approved execution scope.

The native CLI paths were exercised with live bounded tasks after the recorder
upgrade: Claude account continuation and Codex full-wire capture both passed.
That evidence covers those paths only; the remaining rows require implementation
and their own consumer checks.
