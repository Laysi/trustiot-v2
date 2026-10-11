#!/bin/bash
# Final verification of the committed test: burn load (12 busy workers) and an injected stop delay.
S=/tmp/claude-1000/-home-ruisi-projects-trustiot-v2/5d2bbbe4-4241-4ab5-b2fe-2e33d3a23171/scratchpad/issue-259
W=/home/ruisi/projects/trustiot-v2/.claude/worktrees/agent-a22c26ffff5796717
D=$W/target/debug/deps
cd $W
bun $S/burn.ts 12 1500 &
BURN=$!
export PATH=$S/bin:$PATH
STOP_DELAY=0 bun $S/run.ts $D/container-e6f3a737556ef752 10 final-burn-load
STOP_DELAY=3 bun $S/run.ts $D/container-e6f3a737556ef752 2 final-stop-delay-3s
kill $BURN
