#!/usr/bin/env bash
# LIM-140 demo, NEW exec: qualified 0.150.2 codex-exec + empty-success shim — truthful failure, exit 1.
exec bash /tmp/lim140-demo/demo-exec-common.sh new '/srv/work/wt/codex-lhc-0150-2-qual/codex-rs/target/release/codex-exec'
