# S11 — notes/reset compatibility

Implemented the conservative contract for independent review: while LHC is
registered, either explicit token-budget mode or experimental context management
rejects thread startup. Default LHC behavior is unchanged. The gate follows
extension registration (so it recognizes actual LHC ownership) and precedes tool
and model-context exposure. Registered extensions receive normal shutdown.

Evidence and decision:

- `new_context` says a new window starts without summarizing history. Its request
  reaches the strict LHC seam, so it is not a native-clearing bypass; the promise
  is incompatible with reconstruction and compression.
- Experimental activation depends on OpenAI backend routes, OpenAI auth, no
  provider credential overrides, and eligible ChatGPT plans. It enables token
  budget plus the history/notes extension. Direct token-budget activation is
  another route to the incompatible tool and guidance.
- LHC now rejects both opt-in paths before any model or notes request. There is
  no new native fallback and no silent rewriting of inherited reset semantics.
- Two core tests pass: both flags at low/high thresholds, default tool exposure,
  and rejected resume preserving the rollout byte-for-byte with no new request.
- Seven app-server notes tests pass. Two prove authenticated product startup
  rejection over JSON-RPC. Five inherited backend/analytics cases explicitly
  disable LHC capture, preserving their independent extension coverage.
- The existing full-loop tripwire layer now includes core compatibility tests
  and the app-server rejection cases, with zero retries. Ignore count remains
  unchanged; the native-reset ignore inventory points to the active owner.

No configuration fields or schema shapes changed. The supported configuration
is documented in FORK.md and the reader README. Logs: `/tmp/lhc-s11-resume-tests.log`
and `/tmp/lhc-s11-app-tests.log`. Formatting completed. Combined fork qualification
follows S6 before publication.

Final combined qualification passed: [remaining-slices handoff](fork-maintenance-remaining-2026-09-05.md).
