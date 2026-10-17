# Connector / PTY regression fixture corrections

The full workspace run after 7f5251f reported five failures. These corrections
correct test fixtures and expose a private clock seam for deterministic deadline
coverage; production coordination, permissions, deadlines and version checks
remain unchanged.

- Persistent PTY creation still expected the removed default quota of eight.
  The core regression now verifies that the ninth session opens with the correct
  workspace and backing connection and permits input. This follows b036fc7,
  which removed built-in quotas.
- Foreign write-lease handoff queued an Auto command, although Auto deliberately
  does not request a write lease. The fixture now explicitly requests Mutating
  coordination. Its existing foreign-holder exclusion and handoff assertions stay.
- Stalled SFTP timeout expected a handoff lease although newly queued transfers
  default to no lease. The fixture first checks that default, then explicitly
  marks the persisted operation as lease-coordinated. It continues to verify
  the hard deadline despite progress and the bounded post-timeout handoff grace.
- Slow PTY activation relied on completing within forty five-millisecond polls.
  Explicit notifications now hold activation pending while the test observes an
  actual connector heartbeat advance, then release activation and observe its
  completion. A bounded timeout reports a stalled test; no sleep masks a race.
- Streaming output expected a fixed compressed segment budget despite a bounded
  backend channel and timer-driven flushes. Under load the producer can span
  multiple flush windows. Streaming still verifies all 2,000 frames byte-for-byte,
  redaction, compressed row counts, input and closed-session rejection. The same
  chunk and segment budget is checked in a separate deterministic ready-batch
  test through the real coalescer and compressed repository, including sequence
  continuity and exact persisted bytes.

The next full run passed all connector/core tests, but exposed a separate
desktop-stream test race: its 250 ms deadline could expire before the first read.
The stream body now accepts a private per-stream clock; the production wrapper
still uses the same system clock before and after every bounded read. The test
advances its own atomic clock only after a successful first read, then requires
the existing descriptor to fail at the original deadline while the stored
artifact remains valid. No process-global clock, sleep, deadline extension or
permission relaxation is used. The file's pre-existing edits are preserved and
excluded from this commit.

No helper installation, deployment, DNS change or security setting is involved.
Validation in the existing main working tree, retaining unrelated WIP:

- All six initial targeted cases passed, then the additional desktop deadline
  case passed after its clock correction.
- `cargo test --workspace --locked --offline --no-fail-fast`: 1,046 passed,
  zero failed, six existing ignored cases. The run exited successfully.
- The original transfer-file WIP delta is preserved exactly; seven other WIP
  file hashes are unchanged. Only this task's patch is staged for that file.
- Durable operation and source-hash evidence is saved under
  `target/dev-verify/connector-regression.json`.
