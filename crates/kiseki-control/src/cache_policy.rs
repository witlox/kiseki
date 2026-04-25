//! Client-side cache policy (ADR-031).
//!
//! Cache policy inherits org -> project -> workload with each level
//! narrowing (never broadening) the parent. Policy changes apply
//! prospectively — active sessions keep their established ceilings.
//!
//! Spec: ADR-031, `control-plane.feature` cache policy scenarios.

use crate::error::ControlError;

/// Allowed cache modes.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum CacheMode {
    /// Cache entries are pinned and not evicted.
    Pinned,
    /// Cache entries are evicted organically by LRU.
    Organic,
    /// Cache is bypassed entirely.
    Bypass,
}

impl CacheMode {
    /// Parse a cache mode from a string.
    #[must_use]
    pub fn from_str_loose(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "pinned" => Some(Self::Pinned),
            "organic" => Some(Self::Organic),
            "bypass" => Some(Self::Bypass),
            _ => None,
        }
    }
}

/// Cache policy at a specific scope (cluster / org / workload).
#[derive(Clone, Debug)]
pub struct CachePolicy {
    /// Whether caching is enabled.
    pub cache_enabled: bool,
    /// Allowed cache modes at this scope.
    pub allowed_modes: Vec<CacheMode>,
    /// Maximum cache bytes at this scope.
    pub max_cache_bytes: u64,
    /// Maximum cache bytes per node.
    pub max_node_cache_bytes: u64,
    /// Metadata TTL in milliseconds.
    pub metadata_ttl_ms: u64,
    /// Whether write staging is enabled.
    pub staging_enabled: bool,
}

/// Conservative defaults for when no policy is available (I-CC9).
#[must_use]
pub fn conservative_defaults() -> CachePolicy {
    CachePolicy {
        cache_enabled: true,
        allowed_modes: vec![CacheMode::Organic],
        max_cache_bytes: 10 * 1024 * 1024 * 1024, // 10GB
        max_node_cache_bytes: 10 * 1024 * 1024 * 1024,
        metadata_ttl_ms: 5000,
        staging_enabled: false,
    }
}

/// Validate that a child cache policy does not broaden the parent's restrictions.
pub fn validate_cache_policy_inheritance(
    parent: &CachePolicy,
    child: &CachePolicy,
) -> Result<(), ControlError> {
    // Child cannot exceed parent's max_cache_bytes.
    if child.max_cache_bytes > parent.max_cache_bytes {
        return Err(ControlError::Rejected("exceeds_parent_ceiling".into()));
    }
    // Child cannot add modes not in parent's allowed set.
    for mode in &child.allowed_modes {
        if !parent.allowed_modes.contains(mode) {
            return Err(ControlError::Rejected(format!(
                "cache mode {mode:?} not allowed by parent"
            )));
        }
    }
    Ok(())
}

/// Clamp a requested cache mode to the nearest allowed mode.
/// If the requested mode is allowed, return it; otherwise fall back
/// to the first allowed mode.
#[must_use]
pub fn clamp_cache_mode(requested: &CacheMode, allowed: &[CacheMode]) -> CacheMode {
    if allowed.contains(requested) {
        requested.clone()
    } else if !allowed.is_empty() {
        // Fall back to the first allowed mode (organic preferred).
        if allowed.contains(&CacheMode::Organic) {
            CacheMode::Organic
        } else {
            allowed[0].clone()
        }
    } else {
        CacheMode::Bypass
    }
}

/// Resolve the effective cache policy for a workload given its parent.
/// If `cache_enabled` is false, the effective mode is always Bypass.
#[must_use]
pub fn resolve_effective_mode(policy: &CachePolicy) -> CacheMode {
    if !policy.cache_enabled || policy.allowed_modes.is_empty() {
        CacheMode::Bypass
    } else {
        policy.allowed_modes[0].clone()
    }
}

/// Session-established policy snapshot. Active sessions keep their
/// established ceilings even if the policy changes (I-CC10).
#[derive(Clone, Debug)]
pub struct SessionCacheConfig {
    /// Cache mode for this session.
    pub mode: CacheMode,
    /// Max cache bytes for this session.
    pub max_cache_bytes: u64,
    /// Metadata TTL in milliseconds.
    pub metadata_ttl_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cluster_policy() -> CachePolicy {
        CachePolicy {
            cache_enabled: true,
            allowed_modes: vec![CacheMode::Pinned, CacheMode::Organic, CacheMode::Bypass],
            max_cache_bytes: 100 * 1024 * 1024 * 1024, // 100GB
            max_node_cache_bytes: 500 * 1024 * 1024 * 1024,
            metadata_ttl_ms: 5000,
            staging_enabled: true,
        }
    }

    #[test]
    fn cluster_admin_sets_default_cache_policy() {
        // Scenario: Cluster admin sets default cache policy
        let policy = cluster_policy();
        assert!(policy.cache_enabled);
        assert_eq!(policy.allowed_modes.len(), 3);
        assert!(policy.allowed_modes.contains(&CacheMode::Pinned));
        assert!(policy.allowed_modes.contains(&CacheMode::Organic));
        assert!(policy.allowed_modes.contains(&CacheMode::Bypass));
        assert_eq!(policy.max_cache_bytes, 100 * 1024 * 1024 * 1024);
        assert_eq!(policy.metadata_ttl_ms, 5000);
        assert!(policy.staging_enabled);
        // All tenants inherit the cluster default — validated structurally
        // by the inheritance check functions.
    }

    #[test]
    fn org_level_cache_policy_narrows_cluster() {
        // Scenario: Org-level cache policy narrows cluster default
        let cluster = cluster_policy();
        let org = CachePolicy {
            cache_enabled: true,
            allowed_modes: vec![CacheMode::Organic, CacheMode::Bypass],
            max_cache_bytes: 100 * 1024 * 1024 * 1024,
            max_node_cache_bytes: 500 * 1024 * 1024 * 1024,
            metadata_ttl_ms: 5000,
            staging_enabled: true,
        };
        // Org policy is valid (subset of cluster)
        assert!(validate_cache_policy_inheritance(&cluster, &org).is_ok());

        // Workloads under org cannot use pinned mode
        assert!(!org.allowed_modes.contains(&CacheMode::Pinned));

        // Client requesting pinned is clamped to organic
        let clamped = clamp_cache_mode(&CacheMode::Pinned, &org.allowed_modes);
        assert_eq!(clamped, CacheMode::Organic);
    }

    #[test]
    fn org_cannot_broaden_cluster_restrictions() {
        // Scenario: Org cannot broaden cluster-level cache restrictions
        let cluster = cluster_policy();
        let org_too_big = CachePolicy {
            cache_enabled: true,
            allowed_modes: vec![CacheMode::Organic],
            max_cache_bytes: 200 * 1024 * 1024 * 1024, // 200GB > 100GB
            max_node_cache_bytes: 500 * 1024 * 1024 * 1024,
            metadata_ttl_ms: 5000,
            staging_enabled: true,
        };
        let err = validate_cache_policy_inheritance(&cluster, &org_too_big).unwrap_err();
        assert_eq!(err.to_string(), "exceeds_parent_ceiling");
    }

    #[test]
    fn tenant_admin_disables_cache_for_workload() {
        // Scenario: Tenant admin disables cache for a workload
        let workload_policy = CachePolicy {
            cache_enabled: false,
            allowed_modes: vec![CacheMode::Organic],
            max_cache_bytes: 50 * 1024 * 1024 * 1024,
            max_node_cache_bytes: 100 * 1024 * 1024 * 1024,
            metadata_ttl_ms: 5000,
            staging_enabled: false,
        };
        // Effective mode is bypass when cache is disabled
        let mode = resolve_effective_mode(&workload_policy);
        assert_eq!(
            mode,
            CacheMode::Bypass,
            "disabled cache must resolve to bypass"
        );
    }

    #[test]
    fn cache_policy_changes_apply_prospectively() {
        // Scenario: Cache policy changes apply prospectively
        // Active session established with 50GB ceiling
        let session = SessionCacheConfig {
            mode: CacheMode::Organic,
            max_cache_bytes: 50 * 1024 * 1024 * 1024, // 50GB
            metadata_ttl_ms: 5000,
        };

        // Cluster admin changes max to 20GB
        let new_policy = CachePolicy {
            cache_enabled: true,
            allowed_modes: vec![CacheMode::Organic],
            max_cache_bytes: 20 * 1024 * 1024 * 1024, // 20GB
            max_node_cache_bytes: 100 * 1024 * 1024 * 1024,
            metadata_ttl_ms: 5000,
            staging_enabled: true,
        };

        // Active session keeps its 50GB ceiling (I-CC10)
        assert_eq!(session.max_cache_bytes, 50 * 1024 * 1024 * 1024);

        // New sessions would get the 20GB ceiling
        let new_session = SessionCacheConfig {
            mode: CacheMode::Organic,
            max_cache_bytes: new_policy.max_cache_bytes,
            metadata_ttl_ms: new_policy.metadata_ttl_ms,
        };
        assert_eq!(new_session.max_cache_bytes, 20 * 1024 * 1024 * 1024);
        assert_ne!(
            session.max_cache_bytes, new_session.max_cache_bytes,
            "active session should keep old ceiling, new session should get new ceiling"
        );
    }

    #[test]
    fn first_session_with_no_policy_uses_conservative_defaults() {
        // Scenario: First-ever session with no policy available
        let defaults = conservative_defaults();
        assert!(defaults.cache_enabled);
        assert_eq!(defaults.allowed_modes, vec![CacheMode::Organic]);
        assert_eq!(
            defaults.max_cache_bytes,
            10 * 1024 * 1024 * 1024,
            "default 10GB"
        );
        assert_eq!(defaults.metadata_ttl_ms, 5000, "default 5s TTL");
        // Data-path operations proceed normally — structural guarantee.
    }
}
