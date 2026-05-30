#![allow(clippy::unwrap_used, clippy::expect_used)]
//! ADR-047 decoupled-ack — gateway producer wiring.
//!
//! When `decoupled_ack` is on AND the write's surface is async-ack-eligible
//! (S3 / native), `write_impl` mints a perspective-seq, records the intent on
//! a quorum via `put_intent_and_fan`, and FAST-ACKs — skipping the synchronous
//! Raft emit. On any `put_intent_and_fan` error it falls back to the
//! synchronous path (no write is ever lost). POSIX surfaces (NFS / FUSE) are
//! NEVER decoupled even with the gate on.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::ops::{GatewayOps, ReadRequest, WriteRequest, WriteSurface};
use kiseki_log::error::LogError;
use kiseki_log::intent::WriteIntent;
use kiseki_log::shard::{ShardConfig, ShardInfo, ShardState};
use kiseki_log::traits::{
    AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest,
};

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

/// A multi-node-capable log stub: it overrides `put_intent_and_fan` (the
/// default `LogOps` impl returns `Err(Unavailable)`, i.e. "no decoupled-ack
/// here"). `intent_ok` toggles the quorum outcome; both intent calls and the
/// synchronous-emit calls are counted so a test can assert which path ran.
#[derive(Default)]
struct DecoupledSpyLog {
    /// `true` → `put_intent_and_fan` returns Ok (quorum durable);
    /// `false` → returns Err (shortfall) so the gateway falls back.
    intent_ok: AtomicBool,
    /// How many times `put_intent_and_fan` was invoked.
    intent_calls: AtomicUsize,
    /// Recorded intents (so a test can inspect the minted perspective-seq).
    intents: Mutex<Vec<WriteIntent>>,
    /// How many times the synchronous `append_*` emit was invoked.
    sync_calls: AtomicUsize,
}

impl DecoupledSpyLog {
    fn with_intent_ok(ok: bool) -> Self {
        let s = Self::default();
        s.intent_ok.store(ok, Ordering::SeqCst);
        s
    }
}

#[async_trait::async_trait]
impl LogOps for DecoupledSpyLog {
    async fn put_intent_and_fan(
        &self,
        _shard_id: ShardId,
        intent: WriteIntent,
    ) -> Result<(), LogError> {
        self.intent_calls.fetch_add(1, Ordering::SeqCst);
        self.intents.lock().unwrap().push(intent);
        if self.intent_ok.load(Ordering::SeqCst) {
            Ok(())
        } else {
            // Quorum shortfall — the gateway must NOT ack on this; it
            // falls back to the synchronous emit.
            Err(LogError::Unavailable)
        }
    }

    async fn append_delta(&self, _req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        Ok(SequenceNumber(1))
    }

    async fn append_delta_with_forwarding(
        &self,
        _req: AppendDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        Ok(SequenceNumber(1))
    }

    async fn append_chunk_and_delta(
        &self,
        _req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        Ok(SequenceNumber(1))
    }

    async fn append_chunk_and_delta_with_forwarding(
        &self,
        _req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        self.sync_calls.fetch_add(1, Ordering::SeqCst);
        Ok(SequenceNumber(1))
    }

    async fn read_deltas(
        &self,
        _req: ReadDeltasRequest,
    ) -> Result<Vec<kiseki_log::delta::Delta>, LogError> {
        Ok(vec![])
    }

    async fn shard_health(&self, _shard_id: ShardId) -> Result<ShardInfo, LogError> {
        Err(LogError::Unavailable)
    }

    async fn set_maintenance(&self, _shard_id: ShardId, _enabled: bool) -> Result<(), LogError> {
        Ok(())
    }

    async fn truncate_log(&self, _shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        Ok(SequenceNumber(0))
    }

    async fn compact_shard(&self, _shard_id: ShardId) -> Result<u64, LogError> {
        Ok(0)
    }

    fn create_shard(
        &self,
        _shard_id: ShardId,
        _tenant_id: OrgId,
        _node_id: NodeId,
        _config: ShardConfig,
    ) {
    }

    fn update_shard_range(&self, _shard_id: ShardId, _range_start: [u8; 32], _range_end: [u8; 32]) {
    }

    fn set_shard_state(&self, _shard_id: ShardId, _state: ShardState) {}

    fn set_shard_config(&self, _shard_id: ShardId, _config: ShardConfig) {}

    async fn register_consumer(
        &self,
        _shard_id: ShardId,
        _consumer: &str,
        _position: SequenceNumber,
    ) -> Result<(), LogError> {
        Ok(())
    }

    async fn advance_watermark(
        &self,
        _shard_id: ShardId,
        _consumer: &str,
        _position: SequenceNumber,
    ) -> Result<(), LogError> {
        Ok(())
    }
}

/// Build a gateway with `decoupled_ack` set to `enabled`, attached to `log`.
fn setup(log: Arc<dyn LogOps + Send + Sync>, decoupled: bool) -> InMemoryGateway {
    let compositions = CompositionStore::new().with_log(log);
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: test_shard(),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
        tier_policy: Vec::new(),
    });
    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key)
        .with_node_id(7)
        .with_decoupled_ack(decoupled)
        .with_cluster_placement(vec![1, 2, 3])
        .with_target_copies(3)
}

fn write_req(surface: WriteSurface, data: Vec<u8>) -> WriteRequest {
    WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data,
        name: Some("obj".to_owned()),
        conditional: None,
        workflow_ref: None,
        idempotency_key: None,
        forwarded_from_node: None,
        comp_id_override: None,
        tier: None,
        surface,
    }
}

/// Surface eligibility — the predicate that gates the decoupled path.
#[test]
fn surface_async_eligibility() {
    assert!(WriteSurface::S3.is_async_ack_eligible());
    assert!(WriteSurface::Native.is_async_ack_eligible());
    assert!(!WriteSurface::Nfs.is_async_ack_eligible());
    assert!(!WriteSurface::Fuse.is_async_ack_eligible());
}

/// Decoupled ON + S3 + intent quorum Ok: the gateway fast-acks via
/// `put_intent_and_fan` (no synchronous emit) and the data is
/// read-your-writes readable on the SAME gateway.
#[tokio::test(flavor = "multi_thread")]
async fn s3_decoupled_fast_acks_and_is_read_your_writes() {
    let log = Arc::new(DecoupledSpyLog::with_intent_ok(true));
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>, true);

    let plaintext = vec![0xABu8; 4096];
    let resp = gw
        .write(write_req(WriteSurface::S3, plaintext.clone()))
        .await
        .expect("S3 write must fast-ack");

    assert_eq!(
        log.intent_calls.load(Ordering::SeqCst),
        1,
        "decoupled S3 write must record exactly one intent"
    );
    assert_eq!(
        log.sync_calls.load(Ordering::SeqCst),
        0,
        "fast-ack must NOT take the synchronous emit path"
    );
    // The minted perspective-seq must carry this node's id (the tie-break).
    // Extract under a scoped guard so it never lives across the await below.
    let minted_node = {
        let intents = log.intents.lock().unwrap();
        assert_eq!(intents.len(), 1);
        intents[0].perspective_seq.0.node_id
    };
    assert_eq!(minted_node, NodeId(7));

    // Read-your-writes on the same gateway: the composition was created
    // locally before the intent, so the GET resolves without the Raft
    // delta having been incorporated.
    let read = gw
        .read(ReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: resp.composition_id,
            offset: 0,
            length: plaintext.len() as u64,
        })
        .await
        .expect("read-your-writes must resolve the locally-created composition");
    assert_eq!(read.data, plaintext, "decoupled write must round-trip");
}

/// Decoupled ON + S3 + intent quorum Err: the gateway falls back to the
/// synchronous emit so the write still succeeds (no loss).
#[tokio::test(flavor = "multi_thread")]
async fn s3_decoupled_falls_back_on_intent_error() {
    let log = Arc::new(DecoupledSpyLog::with_intent_ok(false));
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>, true);

    let plaintext = vec![0xCDu8; 4096];
    let resp = gw
        .write(write_req(WriteSurface::S3, plaintext.clone()))
        .await
        .expect("write must still succeed via synchronous fallback");

    assert_eq!(
        log.intent_calls.load(Ordering::SeqCst),
        1,
        "the gateway attempts the intent once"
    );
    assert_eq!(
        log.sync_calls.load(Ordering::SeqCst),
        1,
        "on intent shortfall it MUST fall back to the synchronous emit"
    );

    // The write is durable (sync path committed) and read-your-writes holds.
    let read = gw
        .read(ReadRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            composition_id: resp.composition_id,
            offset: 0,
            length: plaintext.len() as u64,
        })
        .await
        .expect("fallback write must be readable");
    assert_eq!(read.data, plaintext);
}

/// Decoupled ON but NFS surface: POSIX close-to-open (ADR-013) is NEVER
/// decoupled. The gateway must NOT call `put_intent_and_fan` and must take
/// the synchronous emit instead.
#[tokio::test(flavor = "multi_thread")]
async fn nfs_surface_never_decoupled_even_when_enabled() {
    let log = Arc::new(DecoupledSpyLog::with_intent_ok(true));
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>, true);

    let plaintext = vec![0xEFu8; 4096];
    gw.write(write_req(WriteSurface::Nfs, plaintext))
        .await
        .expect("NFS write");

    assert_eq!(
        log.intent_calls.load(Ordering::SeqCst),
        0,
        "NFS (POSIX) must NEVER take the decoupled intent path"
    );
    assert_eq!(
        log.sync_calls.load(Ordering::SeqCst),
        1,
        "NFS must take the synchronous emit"
    );
}

/// Decoupled OFF + S3: the capability gate is closed, so even an
/// async-eligible surface takes the synchronous path.
#[tokio::test(flavor = "multi_thread")]
async fn s3_with_gate_off_is_synchronous() {
    let log = Arc::new(DecoupledSpyLog::with_intent_ok(true));
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>, false);

    let plaintext = vec![0x12u8; 4096];
    gw.write(write_req(WriteSurface::S3, plaintext))
        .await
        .expect("S3 write");

    assert_eq!(
        log.intent_calls.load(Ordering::SeqCst),
        0,
        "gate off → no intent path"
    );
    assert_eq!(
        log.sync_calls.load(Ordering::SeqCst),
        1,
        "gate off → synchronous emit"
    );
}
