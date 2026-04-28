//! Erasure-coding fragment distribution primitives (Phase 16b step 6).
//!
//! These are the pure-logic helpers used by the cluster fabric to
//! turn a whole-envelope chunk into per-peer fragments at write time
//! and to reconstruct an envelope from a partial fragment set at read
//! time. The actual data-path wiring (each peer's local store
//! becoming fragment-aware, the `ClusteredChunkStore.write_chunk`
//! branch that picks EC vs Replication-N) requires a `ChunkOps`
//! surface change and lands in a follow-up step.
//!
//! Layered identical to `scrub.rs`:
//!
//! - [`EcStrategy`] — declarative `Replication{N}` vs `EC{X+Y}` enum.
//! - [`encode_for_placement`] — pure function: envelope ciphertext +
//!   strategy + placement → `Vec<FragmentRoute>` with the
//!   `(peer_id, fragment_index, bytes)` mapping.
//! - [`decode_from_responses`] — inverse: given a partial set of
//!   `(fragment_index, bytes)` and the strategy + original length,
//!   reconstruct the envelope ciphertext.
//!
//! Spec: ADR-005 EC defaults table; Phase 16b plan §"EC fragment
//! distribution".

use kiseki_chunk::ChunkError;

/// Durability strategy at the cluster fabric layer. Distinct from
/// `kiseki_chunk::pool::DurabilityStrategy` — that one is per local
/// pool; this one is per cluster-fabric write decision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EcStrategy {
    /// `N` identical copies, one per placement peer. Phase 16a default.
    Replication {
        /// Number of copies — equal to `placement.len()` at write
        /// time.
        copies: u8,
    },
    /// Reed-Solomon `data + parity` fragments, one per placement peer.
    /// `placement.len()` must equal `data + parity` (I-D4 requires
    /// each fragment on a distinct device, which the placement list
    /// enforces).
    Ec {
        /// Data shards.
        data: u8,
        /// Parity shards.
        parity: u8,
    },
}

impl EcStrategy {
    /// Total fragments / copies the strategy expects.
    #[must_use]
    pub fn total_fragments(&self) -> usize {
        match self {
            Self::Replication { copies } => usize::from(*copies),
            Self::Ec { data, parity } => usize::from(*data) + usize::from(*parity),
        }
    }

    /// Minimum fragments required to reconstruct the envelope.
    /// Replication-N: any 1; EC X+Y: any X.
    #[must_use]
    pub fn min_fragments_for_read(&self) -> usize {
        match self {
            Self::Replication { .. } => 1,
            Self::Ec { data, .. } => usize::from(*data),
        }
    }
}

/// One fragment routed to a specific peer at a specific fragment
/// index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentRoute {
    /// Destination peer (or local node) by node id.
    pub peer_id: u64,
    /// Fragment index — meaningful for EC (ranges 0..data+parity);
    /// always 0 for Replication-N.
    pub fragment_index: u32,
    /// Fragment bytes to place on `peer_id`.
    pub bytes: Vec<u8>,
}

/// Errors specific to EC distribution.
#[derive(Debug, thiserror::Error)]
pub enum EcDistributionError {
    /// Placement size doesn't match the strategy's `total_fragments`.
    #[error("placement length {placement} doesn't match strategy total fragments {expected}")]
    PlacementSizeMismatch {
        /// What the placement list provided.
        placement: usize,
        /// What the strategy required.
        expected: usize,
    },
    /// Underlying chunk-error from the EC layer.
    #[error("ec layer: {0}")]
    Chunk(#[from] ChunkError),
}

/// Encode `ciphertext` according to `strategy` and route the
/// resulting fragments to `placement` peers. The result is a flat
/// `Vec<FragmentRoute>` ready for fan-out via `PutFragment`.
///
/// Replication-N: every peer receives the **whole** ciphertext at
/// `fragment_index = 0`.
/// EC X+Y: peer `i` receives shard `i` of the encoded fragment set.
pub fn encode_for_placement(
    strategy: EcStrategy,
    ciphertext: &[u8],
    placement: &[u64],
) -> Result<Vec<FragmentRoute>, EcDistributionError> {
    if placement.len() != strategy.total_fragments() {
        return Err(EcDistributionError::PlacementSizeMismatch {
            placement: placement.len(),
            expected: strategy.total_fragments(),
        });
    }

    match strategy {
        EcStrategy::Replication { .. } => Ok(placement
            .iter()
            .map(|&peer_id| FragmentRoute {
                peer_id,
                fragment_index: 0,
                bytes: ciphertext.to_vec(),
            })
            .collect()),
        EcStrategy::Ec { data, parity } => {
            let encoded = kiseki_chunk::ec::encode(
                ciphertext,
                usize::from(data),
                usize::from(parity),
            )?;
            Ok(placement
                .iter()
                .zip(encoded.fragments)
                .enumerate()
                .map(|(i, (&peer_id, frag))| FragmentRoute {
                    peer_id,
                    fragment_index: u32::try_from(i).unwrap_or(u32::MAX),
                    bytes: frag,
                })
                .collect())
        }
    }
}

/// One fragment retrieved from a peer (or local) during a read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FragmentResponse {
    /// Which fragment index this is.
    pub fragment_index: u32,
    /// Fragment bytes.
    pub bytes: Vec<u8>,
}

/// Reconstruct the envelope ciphertext from a partial set of
/// `responses`. `original_len` is the pre-encode ciphertext length —
/// needed for EC decoding, ignored for Replication-N (just returns
/// the bytes verbatim).
pub fn decode_from_responses(
    strategy: EcStrategy,
    responses: &[FragmentResponse],
    original_len: usize,
) -> Result<Vec<u8>, EcDistributionError> {
    if responses.len() < strategy.min_fragments_for_read() {
        return Err(EcDistributionError::Chunk(ChunkError::ChunkLost));
    }

    match strategy {
        EcStrategy::Replication { .. } => {
            // Any one response is sufficient; first one wins.
            Ok(responses[0].bytes.clone())
        }
        EcStrategy::Ec { data, parity } => {
            let total = usize::from(data) + usize::from(parity);
            let mut slots: Vec<Option<Vec<u8>>> = vec![None; total];
            for r in responses {
                let idx = r.fragment_index as usize;
                if idx < total {
                    slots[idx] = Some(r.bytes.clone());
                }
            }
            let plaintext = kiseki_chunk::ec::decode(
                &mut slots,
                usize::from(data),
                usize::from(parity),
                original_len,
            )?;
            Ok(plaintext)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- strategy boundaries -------------------------------------------------

    #[test]
    fn replication_strategy_totals() {
        let s = EcStrategy::Replication { copies: 3 };
        assert_eq!(s.total_fragments(), 3);
        assert_eq!(s.min_fragments_for_read(), 1);
    }

    #[test]
    fn ec_strategy_totals() {
        let s = EcStrategy::Ec { data: 4, parity: 2 };
        assert_eq!(s.total_fragments(), 6);
        assert_eq!(s.min_fragments_for_read(), 4);
    }

    // --- encode-distribute ---------------------------------------------------

    #[test]
    fn replication_routes_every_peer_to_whole_envelope() {
        let routes = encode_for_placement(
            EcStrategy::Replication { copies: 3 },
            b"hello cluster",
            &[1, 2, 3],
        )
        .unwrap();
        assert_eq!(routes.len(), 3);
        for r in &routes {
            assert_eq!(r.fragment_index, 0, "Replication-N always uses index 0");
            assert_eq!(r.bytes, b"hello cluster");
        }
        assert_eq!(routes[0].peer_id, 1);
        assert_eq!(routes[1].peer_id, 2);
        assert_eq!(routes[2].peer_id, 3);
    }

    #[test]
    fn ec_routes_one_distinct_fragment_per_peer() {
        // Non-uniform payload — a uniform payload would yield identical
        // data shards and zero-parity, masking the distinctness check.
        let payload: Vec<u8> = (0..1024u32).map(|i| (i % 251) as u8).collect();
        let routes = encode_for_placement(
            EcStrategy::Ec { data: 4, parity: 2 },
            &payload,
            &[1, 2, 3, 4, 5, 6],
        )
        .unwrap();
        assert_eq!(routes.len(), 6);
        // Each peer gets a distinct fragment index 0..6.
        let mut indices: Vec<u32> = routes.iter().map(|r| r.fragment_index).collect();
        indices.sort_unstable();
        assert_eq!(indices, vec![0, 1, 2, 3, 4, 5]);
        // Fragments are NOT identical (EC splits data + adds parity).
        assert_ne!(routes[0].bytes, routes[1].bytes);
        assert_ne!(routes[0].bytes, routes[5].bytes);
    }

    #[test]
    fn placement_size_mismatch_is_a_typed_error() {
        let err = encode_for_placement(
            EcStrategy::Ec { data: 4, parity: 2 },
            b"x",
            &[1, 2, 3],
        )
        .expect_err("placement of 3 cannot satisfy 4+2 = 6");
        assert!(matches!(
            err,
            EcDistributionError::PlacementSizeMismatch {
                placement: 3,
                expected: 6,
            }
        ));
    }

    // --- gather-decode -------------------------------------------------------

    #[test]
    fn replication_decode_returns_first_response() {
        let responses = vec![
            FragmentResponse {
                fragment_index: 0,
                bytes: b"hello cluster".to_vec(),
            },
            FragmentResponse {
                fragment_index: 0,
                bytes: b"hello cluster".to_vec(),
            },
        ];
        let out = decode_from_responses(
            EcStrategy::Replication { copies: 3 },
            &responses,
            13,
        )
        .unwrap();
        assert_eq!(out, b"hello cluster");
    }

    #[test]
    fn ec_decode_round_trip() {
        let payload: Vec<u8> = (0..1024u32)
            .map(|i| u8::try_from(i & 0xff).unwrap_or(0))
            .collect();
        let strategy = EcStrategy::Ec { data: 4, parity: 2 };
        let routes = encode_for_placement(strategy, &payload, &[1, 2, 3, 4, 5, 6]).unwrap();

        // Drop two fragments (EC 4+2 tolerates 2 missing).
        let responses: Vec<FragmentResponse> = routes
            .iter()
            .filter(|r| r.fragment_index != 0 && r.fragment_index != 3)
            .map(|r| FragmentResponse {
                fragment_index: r.fragment_index,
                bytes: r.bytes.clone(),
            })
            .collect();
        assert_eq!(responses.len(), 4, "exactly data_shards survive");

        let recovered = decode_from_responses(strategy, &responses, payload.len()).unwrap();
        assert_eq!(recovered, payload, "EC reconstructs exact original");
    }

    #[test]
    fn ec_decode_below_threshold_returns_chunk_lost() {
        let payload = vec![0xCCu8; 256];
        let strategy = EcStrategy::Ec { data: 4, parity: 2 };
        let routes = encode_for_placement(strategy, &payload, &[1, 2, 3, 4, 5, 6]).unwrap();

        // Only 3 fragments: below `data` threshold (4).
        let responses: Vec<FragmentResponse> = routes
            .iter()
            .take(3)
            .map(|r| FragmentResponse {
                fragment_index: r.fragment_index,
                bytes: r.bytes.clone(),
            })
            .collect();

        let err = decode_from_responses(strategy, &responses, payload.len())
            .expect_err("3 < data_shards 4");
        assert!(
            matches!(err, EcDistributionError::Chunk(ChunkError::ChunkLost)),
            "got {err:?}"
        );
    }
}
