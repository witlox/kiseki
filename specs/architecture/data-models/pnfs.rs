//! pNFS layout + Data Server subprotocol types (ADR-038).
//! No method bodies — architecture stubs only.
//!
//! Wire format follows RFC 8435 (Flexible Files Layout) over the
//! existing NFSv4.1 transport defined in `kiseki-gateway::nfs4_server`.

use kiseki_common::ids::{CompositionId, NamespaceId, OrgId};

// =============================================================================
// File handle (pNFS-DS only)
// =============================================================================

/// pNFS-DS file handle. Self-authenticating: the MDS constructs it
/// at LAYOUTGET time with a MAC over its fields; each DS validates
/// the MAC on every op. Stateless on the DS side.
///
/// Spec: ADR-038 §D4 (auth, including §D4.3 wire encoding),
/// §D5 (encryption boundary).
/// Invariants: I-PN1, I-PN2, I-PN3.
///
/// Wire layout (76 bytes total, big-endian for integers, raw UUID
/// bytes for IDs):
///
/// ```text
///   offset  size  field
///        0    16  tenant_id        (uuid::Uuid bytes)
///       16    16  namespace_id     (uuid::Uuid bytes)
///       32    16  composition_id   (uuid::Uuid bytes)
///       48     4  stripe_index     (u32 BE)
///       52     8  expiry_ms        (u64 BE, ms since Unix epoch)
///       60    16  mac              (HMAC-SHA256(K_layout, ...) truncated to 16)
/// ```
///
/// MAC input is `b"kiseki/pnfs-fh/v1\x00" || bytes[0..60]` — the
/// domain-separation tag prevents cross-purpose use of `K_layout`.
pub struct PnfsFileHandle {
    /// Tenant the layout was issued for.
    pub tenant_id: OrgId,
    /// Namespace.
    pub namespace_id: NamespaceId,
    /// Composition this stripe belongs to.
    pub composition_id: CompositionId,
    /// Stripe index within the composition (0-based).
    pub stripe_index: u32,
    /// Wall-clock expiry as ms-since-epoch. DS rejects after this.
    pub expiry_ms: u64,
    /// HMAC-SHA256 truncated to 16 bytes over
    /// `b"kiseki/pnfs-fh/v1\x00" || tenant_id ‖ namespace_id ‖ composition_id ‖ stripe_index_be ‖ expiry_ms_be`.
    /// Key: `HKDF-SHA256(master_key, salt=cluster_id, info=b"kiseki/pnfs-fh/v1")`.
    pub mac: [u8; 16],
}

/// Wire encoding length for `PnfsFileHandle`. RFC 5661 §5 NFS4_FHSIZE
/// max = 128. We use 76 bytes (60-byte payload + 16-byte MAC).
pub const PNFS_FH_BYTES: usize = 76;

/// Domain-separation tag prepended to the MAC input.
/// Spec: ADR-038 §D4.3.
pub const PNFS_FH_MAC_DOMAIN: &[u8] = b"kiseki/pnfs-fh/v1\x00";

// =============================================================================
// Layout (server side, MDS view)
// =============================================================================

/// I/O mode for a layout. Mirrors RFC 5661 §18.43 LAYOUTIOMODE4.
pub enum LayoutIoMode {
    Read,       // LAYOUTIOMODE4_READ = 1
    ReadWrite,  // LAYOUTIOMODE4_RW   = 2
    Any,        // LAYOUTIOMODE4_ANY  = 3
}

/// One stripe in a Flexible Files layout. RFC 8435 §5.1.
pub struct FlexFileStripe {
    /// Byte offset in the composition.
    pub offset: u64,
    /// Stripe length in bytes.
    pub length: u64,
    /// I/O mode this stripe was issued for.
    pub iomode: LayoutIoMode,
    /// Per-stripe file handle the DS will receive.
    pub fh: PnfsFileHandle,
    /// Network address of the DS for this stripe (e.g. "10.0.0.11:2052").
    pub ds_addr: String,
    /// `deviceid4` for this stripe — opaque key into `GETDEVICEINFO`.
    pub device_id: [u8; 16],
}

/// Server-side layout cache entry, keyed by `(composition_id, byte_range)`.
/// Spec: ADR-038 §D6, I-PN4.
pub struct ServerLayout {
    pub layout_type: LayoutType,
    pub composition_id: CompositionId,
    pub stripes: Vec<FlexFileStripe>,
    pub stateid: [u8; 16],
    pub issued_at_ms: u64,
    /// Default 300_000 (5 min) — see `layout_ttl_seconds` in config.
    pub ttl_ms: u64,
}

/// Layout type. RFC 5661 §3.3.13.
pub enum LayoutType {
    /// LAYOUT4_NFSV4_1_FILES = 1 (rejected per ADR-038 §D1).
    Files,
    /// LAYOUT4_FLEX_FILES = 4 (chosen per ADR-038 §D1).
    FlexFiles,
}

// =============================================================================
// GETDEVICEINFO (op 47)
// =============================================================================

/// Server-side response to GETDEVICEINFO (RFC 5661 §18.40 + RFC 8435 §5.2).
/// Resolves `deviceid4` → reachable DS network endpoint(s).
///
/// For Flexible Files we use `ff_device_addr4` which contains a list
/// of `multipath_list4` (RFC 5661 §15.4). Single-mirror layouts have
/// one entry; multipath would be > 1 (not used today).
pub struct DeviceInfo {
    pub device_id: [u8; 16],
    /// One entry per network path to the DS (typically 1).
    pub addresses: Vec<NetAddress>,
    /// FFL versions supported by this DS — currently `[NFSv4_1]`.
    pub versions: Vec<NfsVersion>,
}

/// NFSv4.1 `netaddr4` (RFC 5661 §3.3.9).
pub struct NetAddress {
    /// Network ID per RFC 5665 — `tcp`, `tcp6`, etc.
    pub netid: String,
    /// Universal address — RFC 5665 §5 (e.g. "10.0.0.11.8.4" for port 2052).
    pub uaddr: String,
}

pub enum NfsVersion {
    NfsV4_1,
}

// =============================================================================
// DS subprotocol service surface
// =============================================================================

/// Operations a Data Server endpoint must answer. RFC 8435 §3 +
/// RFC 5661 §13.6 ("DS roles").
///
/// All ops are stateless: the DS validates the fh's MAC + expiry,
/// then translates `(fh4, offset, length)` into a
/// `kiseki_gateway::ops::ReadRequest` / `WriteRequest` and forwards.
///
/// Spec: ADR-038 §D2, §D3.
pub trait DataServerOps: Send + Sync {
    /// Equivalent of NFSv4.1 READ targeting this DS.
    /// Validates fh.mac + expiry; rejects with NFS4ERR_BADHANDLE on failure.
    fn ds_read(&self, fh: &PnfsFileHandle, offset: u64, length: u32) -> DsReadResult;

    /// Equivalent of NFSv4.1 WRITE targeting this DS.
    fn ds_write(&self, fh: &PnfsFileHandle, offset: u64, data: &[u8]) -> DsWriteResult;

    /// COMMIT — for tight-coupled mode this is a no-op (durability
    /// handled by Raft commit on the underlying log). Returns the
    /// MDS's writeverf4 unchanged.
    fn ds_commit(&self, fh: &PnfsFileHandle, offset: u64, length: u64) -> DsCommitResult;

    /// LAYOUTRECALL invalidation hook called by the MDS:
    /// adds `fh` to a recently-revoked LRU. Subsequent ds_read/write
    /// with this fh return NFS4ERR_BADHANDLE until LRU eviction.
    fn ds_invalidate(&self, fh: &PnfsFileHandle);
}

pub struct DsReadResult {
    pub data: Vec<u8>,
    pub eof: bool,
}

pub struct DsWriteResult {
    pub bytes_written: u32,
    /// Per RFC 5661 §18.32 — DATA_SYNC4 (chunks durable, no MDS commit
    /// needed) since we always commit through Raft.
    pub committed: WriteStability,
    pub writeverf: [u8; 8],
}

pub enum WriteStability {
    Unstable,
    DataSync,
    FileSync,
}

pub struct DsCommitResult {
    pub writeverf: [u8; 8],
}

// =============================================================================
// MDS-side layout manager
// =============================================================================

/// MDS-side ops that replace the current `pnfs::LayoutManager`.
///
/// Spec: ADR-038 §D6, §D7.
pub trait LayoutManagerOps: Send + Sync {
    /// LAYOUTGET — RFC 5661 §18.43.
    /// Returns a `ServerLayout` covering at least `[offset, offset+length)`.
    /// The layout's stripes carry MAC'd fh4s suitable for direct DS use.
    fn layout_get(
        &self,
        tenant: OrgId,
        ns: NamespaceId,
        comp: CompositionId,
        offset: u64,
        length: u64,
        iomode: LayoutIoMode,
    ) -> ServerLayout;

    /// LAYOUTRETURN — RFC 5661 §18.44.
    fn layout_return(&self, comp: CompositionId, stateid: &[u8; 16]) -> bool;

    /// GETDEVICEINFO — RFC 5661 §18.40.
    fn get_device_info(&self, device_id: &[u8; 16]) -> Option<DeviceInfo>;

    /// LAYOUTRECALL initiator — broadcasts recall to all session-holders
    /// holding a layout for `comp` and adds the fh4 to each DS's
    /// invalidation LRU. Called from drain/split/merge hooks.
    /// Spec: ADR-038 §D6 (recall triggers).
    fn layout_recall(&self, comp: CompositionId, reason: RecallReason);
}

pub enum RecallReason {
    /// ADR-035 drain hook.
    NodeDraining,
    /// ADR-033 split.
    ShardSplit,
    /// ADR-034 merge.
    ShardMerge,
    /// Cluster CA / fh4 MAC key rotation.
    KeyRotation,
    /// Composition deletion.
    CompositionDeleted,
}

// =============================================================================
// Topology event bus (ADR-038 §D10) — lives in kiseki-control,
// consumed by kiseki-gateway. Resolves ADV-038-3 and -8.
// =============================================================================

/// Cluster-topology change events relevant to outstanding pNFS layouts.
/// Producers emit **after** the corresponding control-Raft commit.
/// Spec: I-PN9.
pub enum TopologyEvent {
    NodeDraining {
        node_id: kiseki_common::ids::NodeId,
        hlc_ms: u64,
    },
    NodeRestored {
        node_id: kiseki_common::ids::NodeId,
        hlc_ms: u64,
    },
    ShardSplit {
        parent: kiseki_common::ids::ShardId,
        children: [kiseki_common::ids::ShardId; 2],
        hlc_ms: u64,
    },
    ShardMerged {
        inputs: Vec<kiseki_common::ids::ShardId>,
        merged: kiseki_common::ids::ShardId,
        hlc_ms: u64,
    },
    CompositionDeleted {
        tenant: OrgId,
        namespace: NamespaceId,
        composition: CompositionId,
        hlc_ms: u64,
    },
    KeyRotation {
        old_key_id: String,
        new_key_id: String,
        hlc_ms: u64,
    },
}

/// Bus surface owned by the control plane runtime.
/// Backed by `tokio::sync::broadcast::Sender<TopologyEvent>` (cap 1024).
pub trait TopologyEventBus: Send + Sync {
    /// Subscribe at startup. Returns a receiver that yields events
    /// emitted after subscription. Lag → cache flush in subscriber
    /// per I-PN9.
    fn subscribe(&self) -> Box<dyn TopologyEventReceiver>;

    /// Emit an event AFTER its underlying control-Raft commit.
    /// Aborted transactions MUST NOT call this.
    fn emit(&self, event: TopologyEvent);
}

pub trait TopologyEventReceiver: Send {
    /// Block until next event, or return Lag if the broadcast channel
    /// overflowed since the last receive. Lag is informational only —
    /// safety is preserved by I-PN4 TTL + cache flush.
    fn recv(&mut self) -> TopologyRecvResult;
}

pub enum TopologyRecvResult {
    Event(TopologyEvent),
    /// Subscriber fell behind; `n` events were dropped. Subscriber
    /// must invalidate its layout cache on receipt.
    Lag(u64),
    /// Sender closed.
    Closed,
}
