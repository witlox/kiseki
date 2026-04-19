# BDD Harness Plan — kiseki-acceptance

**Status**: Planned. **Created**: 2026-04-19.

## Architecture

Following the pact-acceptance pattern:

### Rust (kiseki-acceptance crate)

```
crates/kiseki-acceptance/
├── Cargo.toml              # harness = false, depends on domain crates
├── features/               # symlink to specs/features/ (or copy)
└── tests/
    ├── acceptance.rs        # World struct + cucumber runner
    └── steps/
        ├── mod.rs           # module declarations
        ├── helpers.rs       # shared test utilities
        ├── log.rs           # log.feature steps
        ├── chunk.rs         # chunk-storage.feature steps
        ├── crypto.rs        # key-management.feature steps
        ├── composition.rs   # composition.feature steps
        ├── view.rs          # view-materialization.feature steps
        ├── transport.rs     # authentication.feature steps
        ├── gateway.rs       # protocol-gateway.feature steps (stubs)
        ├── client.rs        # native-client.feature steps (stubs)
        ├── advisory.rs      # workflow-advisory.feature steps
        └── operational.rs   # operational.feature steps
```

### Go (control/tests/acceptance/)

Following the ghyll pattern:

```
control/tests/acceptance/
├── acceptance_test.go       # godog TestFeatures + InitializeScenario
├── state.go                 # ControlWorld shared state
├── steps_tenant.go          # control-plane.feature tenant steps
├── steps_iam.go             # control-plane.feature IAM steps
├── steps_policy.go          # control-plane.feature policy steps
└── steps_advisory.go        # control-plane.feature advisory steps
```

## KisekiWorld struct

```rust
#[derive(World)]
#[world(init = Self::new)]
pub struct KisekiWorld {
    // Real implementations
    pub log_store: MemShardStore,
    pub key_store: MemKeyStore,
    pub audit_log: AuditLog,
    pub chunk_store: ChunkStore,
    pub composition_store: CompositionStore,
    pub view_store: ViewStore,
    pub advisory_table: WorkflowTable,
    pub budget_enforcer: BudgetEnforcer,

    // Test state
    pub last_error: Option<String>,
    pub last_sequence: Option<u64>,
    pub last_epoch: Option<u64>,
    // ... per-feature test state
}
```

## Step definition pattern

```rust
#[given("a Kiseki cluster with 5 storage nodes")]
async fn given_cluster(world: &mut KisekiWorld) {
    // Setup is in World::new() — this is a no-op acknowledgment
}

#[given(regex = r#"^shard "(\w+)" is healthy with all 3 replicas online$"#)]
async fn given_shard_healthy(world: &mut KisekiWorld, shard_name: String) {
    world.create_test_shard(&shard_name);
}

#[when(regex = r#"^the Composition context appends a delta with:$"#)]
async fn when_append_delta(world: &mut KisekiWorld, step: &Step) {
    let table = step.table.as_ref().unwrap();
    // Parse table rows → AppendDeltaRequest
    let result = world.log_store.append_delta(req);
    // Store result for THEN steps
}
```

## Scenarios that CAN be tested now

These use in-memory stores and don't need Raft/gRPC/network:

| Feature file | Testable now | Need infra | Total |
|-------------|-------------|------------|-------|
| log.feature | 10 | 11 | 21 |
| chunk-storage.feature | 8 | 17 | 25 |
| key-management.feature | 6 | 11 | 17 |
| composition.feature | 10 | 11 | 21 |
| view-materialization.feature | 8 | 15 | 23 |
| authentication.feature | 4 | 12 | 16 |
| protocol-gateway.feature | 0 | 23 | 23 |
| native-client.feature | 2 | 24 | 26 |
| operational.feature | 5 | 28 | 33 |
| workflow-advisory.feature | 7 | 44 | 51 |
| control-plane.feature (Go) | 10 | 22 | 32 |
| **Total** | **70** | **218** | **288** |

Untestable scenarios are marked as pending (cucumber shows them as
"skipped" — not failures).

## Execution order

1. Create `kiseki-acceptance` crate with `World` and runner
2. Wire each feature file — start with steps that match existing tests
3. Add steps that exercise NEW behavior (TDD: write step, see fail, implement)
4. Check if scenarios need extending (gaps in existing features)
5. Create Go acceptance harness for control-plane.feature
