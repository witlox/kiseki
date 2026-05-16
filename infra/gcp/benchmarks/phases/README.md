# Bench phase scripts

Each `.sh` in this dir is one perf phase, runnable standalone via the
`bench` driver (`../bench`). Phases use a `NN-name.sh` convention
where `NN` is a 2-digit ordinal driving `bench list` / `bench run all`
order.

## Contract

Each script:

1. Sources `../perf-common.sh` to inherit `$RESULTS`,
   `$ALL_STORAGE`, `discover_leader`, and the metric / fio helpers.
2. Writes its output files under `$RESULTS/` — typically
   `$RESULTS/<phase-name>.txt` plus any auxiliary metric snapshots.
3. Exits with:
   - `0` — phase completed (raw numbers should be checked; "low
     throughput" is NOT a halt — only "stop the run" conditions are).
   - `2` — phase detected a wedge or functional break (errors > 0,
     hydrator backlog growing, fio in `D` state past the runtime cap).
     The `bench` driver halts the remaining phases.
   - Anything else — a logic bug in the script itself.

## Adding a new phase

1. Pick the smallest 2-digit ordinal that places the script where it
   should run in `bench run all`.
2. `cp 00-health.sh <new-name>.sh` (when 00-health.sh lands in #134).
3. Edit. Sourcing perf-common.sh is enough — `$RESULTS`,
   `$LEADER_HOST`, `$CLIENT_ARRAY` etc. all come for free.
4. `bench list` to confirm it shows up.
5. Run standalone first: `bench run <new-name>`.

## See also

- `../bench` — driver
- `../perf-common.sh` — shared helpers + `$RESULTS` lifecycle
- `../../scripts/setup-bench-ctrl.sh` — stages this dir on bench-ctrl
  (issue #54)
