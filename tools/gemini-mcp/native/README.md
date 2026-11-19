# Gemini native bridge contract

Invoke `/usr/bin/xcrun swift tools/gemini-mcp/native/bridge.swift <command>`.
`probe`, `capabilities`, `usage`, `usage_refresh`, and `usage_focus` take
no input. `focus` accepts EOF or `{}` for idle focus, or a JSON request binding
`request_id`, `session_id`, and `state_dir` for a matching active lease. Other
commands read one UTF-8 JSON object. Events are version-1
NDJSON and carry the bound request and session identifiers.

- `run`: request `request_id`, UUID `session_id`, private absolute
  `state_dir`, `timeout_ms`, `prompt`, and optional `capability`:
  `chat`, `image`, `video`, `deep_research`, or `canvas`. Media and
  research modes are experimental and must match the actual tool menu and
  composer. A fresh chat after an owned turn may use `prior_request_id`;
  the previous turn must still be the latest and match its receipt.
- `continue`: the same fields plus `prior_request_id` in the same session
  and mode. Previous output identity must still match. Canvas and research
  continuations verify the previous panel was closed before opening the new
  owned card; the new text may be identical. This submits a new turn and never
  replays an old prompt.
- `resume` / `cancel`: `request_id`, `session_id`, `state_dir`,
  `timeout_ms`; observe/reconcile or stop the owned turn. Resume never clicks
  Stop. Cancel can close only a persisted owned Save or Go to Folder panel via
  its exact Cancel control, then verifies panel disappearance, the same owned
  turn, and no active generation before releasing its lease. Unknown actions
  are not replayed and downloaded files are not deleted.
- `inspect`: report exact-label candidates within the latest owned answer
  and, when a research plan can be bound, its `pending_action`.
- `confirm`: supply the usual binding plus `action_id` and
  `action_version` from that pending action. Only the exact owned
  `deep_research_start` plan is actionable; the full plan fingerprint is
  rechecked after durable intent and before one press. A lost action result
  is unknown and never automatically repeated.
- `collect`: observe the original bound media operation or replay its
  verified terminal receipt. A save-panel path is implemented for a uniquely
  bound generated-media download, an attached native Save sheet, and an owned
  `state_dir/artifacts/` destination. The sheet URL must prove the directory;
  when needed, a scoped Go to Folder dialog is attempted once. The result
  must be a private regular file with matching MIME, signature, size, SHA-256
  and native decode checks. Unrecognized UI, ambiguous source, and uncertain
  actions remain unknown; a preview or arbitrary download path is not an
  original artifact.
- `usage` reads only an explicitly identified current native Usage limits
  section. `usage_refresh` requires the app to be foreground and idle, uses
  the shared lease/lock, and attempts exact Settings, Usage limits, and Reload
  controls. `usage_focus` explicitly focuses the app only after the same idle
  checks. The response source is
  `gemini_desktop_usage_limits_current_view`, with collection time,
  `buckets`, and upstream update/reset labels when observed. `ui_restored` and
  `ui_restore_reason` describe whether an explicitly opened, newly created
  Settings window was closed and the original focus restored. Preexisting
  Settings windows are never closed. A bucket needs a directly bound row label
  and a single metric; ambiguous rows return unknown. `collected_at` is not the
  upstream refresh time. Account and
  per-capability scope remain unknown.

Canvas and research completion require a freshly verified prior panel baseline,
the latest owned result card, a window/PID/foreground and hit-tested card click,
and two matching complete AX document values. Completion emits
`completion_evidence` with kind `canvas_document_text` or
`research_report_text`, bound request/session, and the UTF-8 text SHA-256.
Research additionally requires a confirmed plan, unless a continuation inherits
the verified previous plan and no new plan is shown. Local title/content
identity is not a provider artifact ID. An older binding without the baseline
may establish it by closing a uniquely identified native Canvas-specific
control and verifying that its panel disappeared; a generic Close control is
not sufficient. If this proof is unavailable the result remains unknown and
no lease is force-released.

The native bridge coordinates Gemini Desktop via a private GUI lock and active lease in `~/Library/Caches/GeminiDesktopMCP`. An explicit
`not_submitted: true` means durable proof that no send occurred and the lease
was released. Terminal receipts use mode 0600; state directories must be owned
by the current user without group/other access. Input attachments and
non-default model selection are not implemented.

Real owned text chat and continuation were previously observed. One image
request received a complete refusal and was persisted as
`failed / native_error_response`; this does not establish an account or quota
cause. Canvas mode selection, submission and a visible three-item document were
observed, but the original binding lacks a saved blank-panel baseline.
The newer media export, research confirmation/report, Canvas extraction,
continuation, and usage navigation paths are implemented but have not yet
received unified runtime validation. Selectors and system-panel layouts must be
checked on the installed Gemini version; the adapter fails closed when they
are not observed.
