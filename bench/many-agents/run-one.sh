#!/bin/bash
# One 64-agent run, same 40-minute window and cold CI as every other team, optionally after the run in progress
# on this machine.   bash run-one.sh <avp|gitci> <run> <model> <effort> <OpenAI $ counted as spent elsewhere>
S=$HOME/.ctx-bench/scale; R=$HOME/.ctx-bench/done-runs; L=$S/big.log; export BENCH_BASE_REPO=/root/bench/deleg-bench.git
while ps -eo args | grep -q "^bash /root/.ctx-bench/scale/run-done.sh"; do sleep 20; done
echo "$(date -u +%H:%M) $2 starts on $(hostname) (\$$5 counted as spent on OpenAI)" >> $L
BENCH_CI=cold BENCH_CODEX_MODEL=$3 BENCH_CODEX_EFFORT=$4 BENCH_RUN=$2 BENCH_SPENT=$5 BENCH_BRAKE=255 BENCH_TOTAL_MIN=40 BENCH_LIMIT_MIN=40 \
  BENCH_FLOW=$S/out/64/flow BENCH_TAG=deleg-T64-base BENCH_ACCEPT=$S/out/64/acceptance.test.ts bash $S/run-done.sh $1 64 > $S/done-$2.out 2>&1
(cd $R && python3 -I $S/analyse-done.py $2 | head -1) >> $L 2>&1
echo "$(date -u +%H:%M) $2 OVER" >> $L
