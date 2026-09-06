#!/bin/sh
# Regenerate the LHC core-touchpoint patch series from the ONE upstream base
# recorded in patches/lhc/BASE. Each group below is an explicit file list; a
# fork-owned file belongs to exactly one group. Tripwire layer 4 applies the
# series at BASE and requires byte-identity with the tree plus full coverage
# of fork-owned files under codex-rs/, so an omitted file fails there.
#
# Prerequisite: `git diff <base>` only sees tracked files, so `git add` any
# newly created hook file before running this. The script reads the working
# tree and never touches the caller's index.
#
# Usage (from anywhere in the repo): sh patches/lhc/regenerate.sh
set -eu
cd "$(git rev-parse --show-toplevel)"
BASE=$(git rev-parse --verify "$(cat patches/lhc/BASE)^{commit}")

# regen <patch-file> <file>... : one diff from BASE for one ownership group.
regen() {
  out=$1
  shift
  git diff "$BASE" -- "$@" > "patches/lhc/$out"
}

regen 0001-workspace-member.patch \
  codex-rs/Cargo.toml \
  codex-rs/cli/tests/version.rs \
  codex-rs/app-server-daemon/README.md \
  codex-rs/app-server-daemon/src/lib.rs \
  codex-rs/app-server-daemon/src/managed_install.rs \
  codex-rs/app-server-daemon/src/managed_install_tests.rs \
  codex-rs/app-server-daemon/src/update_loop.rs \
  codex-rs/app-server-daemon/src/update_loop_tests.rs

regen 0002-raw-item-contributor.patch \
  codex-rs/ext/extension-api/src/contributors.rs \
  codex-rs/ext/extension-api/src/contributors/raw_item.rs \
  codex-rs/ext/extension-api/src/contributors/turn_lifecycle.rs \
  codex-rs/ext/extension-api/src/lib.rs \
  codex-rs/ext/extension-api/src/registry.rs \
  codex-rs/ext/goal/tests/goal_extension_backend.rs

regen 0003-feature-flag.patch \
  codex-rs/features/src/lib.rs \
  codex-rs/core/config.schema.json

regen 0004-session-raw-item-hook.patch \
  codex-rs/core/src/session/mod.rs \
  codex-rs/core/src/session/inject.rs \
  codex-rs/core/src/session/lhc_capture_e2e_tests.rs \
  codex-rs/core/src/stream_events_utils.rs \
  codex-rs/core/src/compact.rs

regen 0005-app-server-dep.patch \
  codex-rs/app-server/Cargo.toml

regen 0006-app-server-install.patch \
  codex-rs/app-server/src/extensions.rs

regen 0007-lhc-compact-arm.patch \
  codex-rs/core/Cargo.toml \
  codex-rs/.config/nextest.toml \
  codex-rs/core/src/compact_lhc/preparation.rs \
  codex-rs/core/src/compact_lhc/workers.rs \
  codex-rs/core/src/compact_lhc/installation.rs \
  codex-rs/core/src/compact_lhc.rs \
  codex-rs/core/src/compact_lhc_worker_error.rs \
  codex-rs/core/src/compact_lhc_tests.rs \
  codex-rs/core/src/compact_lhc_strict_routing_tests.rs \
  codex-rs/core/src/compact_lhc_slice_d_tests.rs \
  codex-rs/core/src/compact_lhc_mid_turn_tests.rs \
  codex-rs/core/src/compact_lhc_canary_tests.rs \
  codex-rs/core/src/compact_lhc_readiness_tests.rs \
  codex-rs/core/src/lc_adaptive_service_tier.rs \
  codex-rs/core/src/lhc_inference_bridge.rs \
  codex-rs/core/src/lib.rs \
  codex-rs/core/src/thread_manager.rs \
  codex-rs/core/src/config/mod.rs \
  codex-rs/core/src/config/config_tests.rs \
  codex-rs/core/tests/suite/compact.rs \
  codex-rs/core/tests/suite/compact_lhc_mid_turn_loops.rs \
  codex-rs/core/tests/suite/compact_lhc_image_tests.rs \
  codex-rs/core/tests/suite/compact_lhc_context_management.rs \
  codex-rs/core/tests/suite/compact_remote.rs \
  codex-rs/core/tests/suite/compact_remote_parity.rs \
  codex-rs/core/tests/suite/compact_resume_fork.rs \
  codex-rs/core/tests/suite/client.rs \
  codex-rs/core/tests/suite/current_time_reminder.rs \
  codex-rs/core/tests/suite/hooks.rs \
  codex-rs/core/tests/suite/lhc_preturn_readiness.rs \
  codex-rs/core/tests/suite/pending_input.rs \
  codex-rs/core/tests/suite/remote_env.rs \
  codex-rs/core/tests/suite/responses_lite.rs \
  codex-rs/core/tests/suite/retry_after.rs \
  codex-rs/core/tests/suite/rollout_budget.rs \
  codex-rs/core/tests/suite/token_budget.rs \
  codex-rs/core/tests/suite/tools.rs \
  codex-rs/core/tests/suite/window_headers.rs \
  codex-rs/core/tests/suite/model_switching.rs \
  codex-rs/core/tests/suite/mod.rs \
  codex-rs/core/src/session/turn.rs \
  codex-rs/core/src/session/session.rs \
  codex-rs/core/src/session/tests.rs \
  codex-rs/core/src/session/input_queue.rs \
  codex-rs/core/src/session/lhc_band_shape_eval_tests.rs \
  codex-rs/core/src/state/auto_compact_window.rs \
  codex-rs/core/src/state/session.rs \
  codex-rs/core/src/state/service.rs \
  codex-rs/core/src/tasks/compact.rs \
  codex-rs/core/src/tasks/lifecycle.rs \
  codex-rs/core/src/tasks/mod.rs \
  codex-rs/exec/src/lib.rs \
  codex-rs/exec/src/lib_tests.rs \
  codex-rs/exec/src/event_processor_with_human_output_tests.rs \
  codex-rs/exec/tests/suite/apply_patch.rs \
  codex-rs/exec/tests/suite/auth_env.rs \
  codex-rs/exec/tests/suite/resume.rs \
  codex-rs/config/src/config_toml.rs \
  codex-rs/protocol/src/config_types.rs \
  codex-rs/protocol/src/openai_models.rs \
  codex-rs/models-manager/models.json \
  codex-rs/models-manager/src/manager.rs \
  codex-rs/models-manager/src/manager_tests.rs \
  codex-rs/history/src/lib.rs \
  codex-rs/history/src/ordinal.rs \
  codex-rs/rollout/src/lib.rs \
  codex-rs/rollout/src/ordinal.rs \
  codex-rs/rollout/src/recorder.rs \
  codex-rs/rollout/src/recorder_tests.rs \
  codex-rs/state/src/migrations.rs \
  codex-rs/state/src/migrations_tests.rs \
  codex-rs/state/src/sqlite.rs \
  codex-rs/state/thread_history_migrations/0007_rollout_generation_id.sql \
  codex-rs/thread-store/Cargo.toml \
  codex-rs/thread-store/src/live_thread.rs \
  codex-rs/thread-store/src/local/mod.rs \
  codex-rs/thread-store/src/local/live_writer.rs \
  codex-rs/thread-store/src/local/rollout_migration.rs \
  codex-rs/thread-store/src/local/rollout_migration_tests.rs \
  codex-rs/thread-store/src/local/thread_history.rs \
  codex-rs/thread-store/src/local/thread_history_generation.rs \
  codex-rs/thread-store/src/local/thread_history_materialization.rs \
  codex-rs/thread-store/src/local/thread_history_materialization_tests.rs \
  codex-rs/app-server/src/request_processors/thread_processor.rs \
  codex-rs/app-server/tests/suite/v2/history_notes_extension.rs

# Release identity and update wiring (maintenance slices 01-06): the embedded
# fork release, CLI/TUI update execution through the fork installer, fork
# release discovery, and doctor's update row.
regen 0008-release-identity-update-wiring.patch \
  BUILD.bazel \
  codex-rs/install-context/BUILD.bazel \
  codex-rs/install-context/src/lhc_release.rs \
  codex-rs/install-context/src/lhc_release_tests.rs \
  codex-rs/install-context/src/lib.rs \
  codex-rs/cli/src/main.rs \
  codex-rs/cli/src/doctor/updates.rs \
  codex-rs/cli/src/doctor/output.rs \
  codex-rs/tui/src/update_action.rs \
  codex-rs/tui/src/update_action_tests.rs \
  codex-rs/tui/src/update_prompt.rs \
  codex-rs/tui/src/update_versions.rs \
  codex-rs/tui/src/updates.rs \
  codex-rs/tui/src/updates_cache.rs \
  codex-rs/tui/src/updates_cache_tests.rs \
  codex-rs/tui/src/app/exit_summary.rs \
  codex-rs/tui/src/history_cell/mod.rs \
  codex-rs/tui/src/history_cell/notices.rs \
  codex-rs/tui/src/history_cell/tests.rs \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__lhc_unix_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__pnpm_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__standalone_unix_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__standalone_windows_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__unmanaged_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/history_cell/snapshots/codex_tui__history_cell__tests__vite_plus_update_available_history_cell_snapshot.snap \
  codex-rs/tui/src/snapshots/codex_tui__update_prompt__tests__update_prompt_modal.snap

echo "regenerated $(ls patches/lhc/0*.patch | wc -l) patches from $BASE"
