#!/bin/bash
# Regression runs: same injected `podman stop` delay (see bin/podman) for the old and new test.
S=/tmp/claude-1000/-home-ruisi-projects-trustiot-v2/5d2bbbe4-4241-4ab5-b2fe-2e33d3a23171/scratchpad/issue-259
W=/home/ruisi/projects/trustiot-v2/.claude/worktrees/agent-a22c26ffff5796717
D=$W/target/debug/deps
cd $W
export PATH=$S/bin:$PATH
STOP_DELAY=1 bun $S/run.ts $D/container_old-adab6535c1bf9e85 5 old-stop-delay-1s
STOP_DELAY=1 bun $S/run.ts $D/container-e6f3a737556ef752 5 fixed-stop-delay-1s
STOP_DELAY=3 bun $S/run.ts $D/container_old-adab6535c1bf9e85 5 old-stop-delay-3s
STOP_DELAY=3 bun $S/run.ts $D/container-e6f3a737556ef752 5 fixed-stop-delay-3s
STOP_DELAY=0 bun $S/run.ts $D/container_old-adab6535c1bf9e85 20 old-natural-load
