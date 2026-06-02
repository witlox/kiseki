# ADR-049 storage migration runbook

How to move a fjall keyspace from one path to another after a
placement-policy change. Phase 6 of ADR-049 ships this as a
`kiseki-admin storage migrate` command; until then the steps below
are the supported operator path.

## When you need this

You changed `PlacementPolicy.tiers[SmallObject].preferences` (or
any other tier's preferences) via `kiseki-admin topology
placement-policy set-store-prefs`. The placement policy revision
bumps in the catalog state machine. Each node's resolver computes
a new path for the affected tier on next boot. But the fjall
keyspace is still at the **prior** path — the actual bytes haven't
moved.

If a node reboots without migrating, I-CP-Move (ADR-049 §D8.1)
trips and the node refuses to start with a clear error:

```
ADR-049 I-CP-Move tripped on tier small_object:
  prior=/mnt/sata0/kiseki/small-object
  resolved=/mnt/nvme0/kiseki/small-object
  Run `kiseki-admin storage migrate --tier=small_object --node=<this-node>`
  before retrying.
```

## What `storage migrate` does

1. Quiesces writes to the tier on this node (sets it read-only in
   the in-process gateway state).
2. Drains any in-flight `fsync_pending` chain (ADR-046).
3. Copies the fjall keyspace `prior_path → resolved_path` using
   `rsync -a --delete`. The keyspace is closed during the copy so
   the fjall WAL has no concurrent writers.
4. Atomically updates
   `$KISEKI_DATA_DIR/kiseki-tier-paths.json` with the new path.
5. Re-opens the fjall store at the new path.
6. Clears the read-only bit.

## Manual procedure (until phase 6 admin command ships)

On the affected node:

```bash
# 1. Stop kiseki-server so no fjall writes race the move.
sudo systemctl stop kiseki-server

# 2. Identify the prior + resolved paths from the error log AND
#    from the catalog. The catalog is the source of truth for
#    the resolved path; `kiseki-tier-paths.json` is the source of
#    truth for the prior.
PRIOR=$(jq -r '.paths.small_object' \
    "$KISEKI_DATA_DIR/kiseki-tier-paths.json")
RESOLVED=$(kiseki-admin topology node-inventory show --node=$NODE_ID \
    | yq -r '.resolved.small_object')
echo "Migrating small_object: $PRIOR → $RESOLVED"

# 3. Make sure the destination's parent exists.
sudo mkdir -p "$(dirname "$RESOLVED")"

# 4. rsync the keyspace. `-a` preserves perms + timestamps; `--delete`
#    cleans stray files in destination.
sudo rsync -av --delete "$PRIOR/" "$RESOLVED/"

# 5. Update the pointer file atomically (tmp + rename).
TMP=$(mktemp "$KISEKI_DATA_DIR/kiseki-tier-paths.json.XXXXXX")
jq --arg path "$RESOLVED" '.paths.small_object = $path' \
    "$KISEKI_DATA_DIR/kiseki-tier-paths.json" > "$TMP"
sudo chmod 0600 "$TMP"
sudo mv "$TMP" "$KISEKI_DATA_DIR/kiseki-tier-paths.json"

# 6. Restart kiseki-server. I-CP-Move now sees pointer == resolved
#    → opens at the new path.
sudo systemctl start kiseki-server

# 7. (Optional) Once boot is healthy, reclaim the old path:
sudo rm -rf "$PRIOR"
```

Repeat for each tier whose `chosen_mount` changed (verify via
`kiseki-admin topology node-inventory show --node=$NODE_ID`).

## Verify

After the restart:

```bash
# Confirm I-CP-Move did NOT trip.
journalctl -u kiseki-server | grep "ADR-049 I-CP-Move"
# Should be empty.

# Confirm phase 5a boot succeeded with the new pointer file.
journalctl -u kiseki-server | grep "ADR-049 phase 5a boot"
# Should report `catalog populated, pointer file saved`.

# Confirm the catalog sees this node at its new resolved path.
kiseki-admin topology node-inventory show --node=$NODE_ID \
    | grep small_object
```

## Cluster-wide rollout

For a 3+ node cluster, migrate one node at a time. Cluster
redundancy (R-3 / EC-4+2) keeps the tier available while
individual nodes are stopped + migrated. Wait for each node to
fully rejoin (per-shard Raft + control-plane) before moving to
the next.

```bash
for NODE in 1 2 3 4 5 6; do
    ssh kiseki-${NODE} 'sudo systemctl stop kiseki-server'
    ssh kiseki-${NODE} 'bash /path/to/migrate-small-object.sh'
    ssh kiseki-${NODE} 'sudo systemctl start kiseki-server'
    # Wait for the node to rejoin (poll /cluster/info).
    while ! curl -fs "kiseki-${NODE}:9090/cluster/info" | jq '.node_state=="active"'; do
        sleep 5
    done
done
```

## What if the migration is interrupted

If you stop the script after step 4 (rsync done) but before step
5 (pointer-file update), the next boot will trip I-CP-Move
(pointer still says old path, but both old AND new paths now have
keyspaces). Recovery: re-run step 5 manually, then start
kiseki-server.

If you stop after step 5 but before step 6 (cleanup), the next
boot will succeed (pointer matches resolved); the old path
lingers as harmless garbage until cleaned up.

## Related

- ADR-049 §D8 (operator-driven migration v1)
- ADR-049 §D8.1 (I-CP-Move enforcement)
- ADR-049 specs/features/device-inventory.feature scenarios DI-4 +
  DI-4b (BDD acceptance for migration + non-quiesced reboot)
- `crates/kiseki-server/src/cluster_control/tier_paths.rs`
  (`save()` writes the pointer atomically; `compare_tier()` is
  the I-CP-Move enforcement)
- `crates/kiseki-server/src/cluster_control/phase5_boot.rs`
  (calls `tier_paths::save` after successful resolve)
