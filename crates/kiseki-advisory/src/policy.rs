//! Advisory policy enforcement types.
//!
//! Profile allow-lists, budget hierarchy ceilings, retention-hold guards,
//! priority caps, prefetch budgets, scope validation, forbidden-field
//! checks, draining FSM, and policy revocation logic.

use std::collections::HashSet;

use kiseki_common::advisory::{
    ClientId, PoolDescriptor, PoolHandle, Priority, RetentionIntent, WorkloadProfile,
};

use crate::error::AdvisoryError;

// =============================================================================
// Profile allow-list
// =============================================================================

/// Allow-list of workload profiles permitted for a workload (I-WA7).
#[derive(Clone, Debug)]
pub struct ProfileAllowList {
    allowed: HashSet<WorkloadProfile>,
}

impl ProfileAllowList {
    /// Create a new allow-list from allowed profiles.
    #[must_use]
    pub fn new(profiles: &[WorkloadProfile]) -> Self {
        Self {
            allowed: profiles.iter().copied().collect(),
        }
    }

    /// Check whether a profile is allowed.
    pub fn check(&self, profile: WorkloadProfile) -> Result<(), AdvisoryError> {
        if self.allowed.contains(&profile) {
            Ok(())
        } else {
            Err(AdvisoryError::ProfileNotAllowed(format!("{profile:?}")))
        }
    }

    /// Remove a profile (policy narrowing, I-WA18).
    pub fn revoke(&mut self, profile: WorkloadProfile) {
        self.allowed.remove(&profile);
    }

    /// Check whether a profile is still in the allow-list.
    #[must_use]
    pub fn contains(&self, profile: &WorkloadProfile) -> bool {
        self.allowed.contains(profile)
    }
}

// =============================================================================
// Budget hierarchy ceiling
// =============================================================================

/// Budget ceiling for a scope level (org / project / workload).
#[derive(Clone, Debug)]
pub struct BudgetCeiling {
    /// Max `hints_per_sec` at this scope.
    pub hints_per_sec: u32,
}

impl BudgetCeiling {
    /// Validate that a child budget does not exceed its parent ceiling.
    pub fn validate_child(&self, child_hints_per_sec: u32) -> Result<(), AdvisoryError> {
        if child_hints_per_sec > self.hints_per_sec {
            Err(AdvisoryError::ChildExceedsParentCeiling(format!(
                "child {} > parent {}",
                child_hints_per_sec, self.hints_per_sec
            )))
        } else {
            Ok(())
        }
    }
}

// =============================================================================
// Retention hold guard
// =============================================================================

/// Guard that prevents advisory hints from bypassing retention holds (I-WA14).
pub struct RetentionHoldGuard {
    has_hold: bool,
}

impl RetentionHoldGuard {
    /// Create a guard for a composition with or without a retention hold.
    #[must_use]
    pub fn new(has_hold: bool) -> Self {
        Self { has_hold }
    }

    /// Check whether a retention intent hint is compatible with the hold.
    pub fn check_intent(&self, intent: RetentionIntent) -> Result<(), AdvisoryError> {
        if self.has_hold && intent == RetentionIntent::Temp {
            Err(AdvisoryError::RetentionPolicyConflict)
        } else {
            Ok(())
        }
    }
}

// =============================================================================
// Priority cap
// =============================================================================

/// Priority policy cap for a workload (I-WA14).
#[derive(Clone, Debug)]
pub struct PriorityCap {
    max_priority: Priority,
    allowed: HashSet<Priority>,
}

impl PriorityCap {
    /// Create a cap with the given maximum allowed priority.
    #[must_use]
    pub fn new(max_priority: Priority) -> Self {
        let mut allowed = HashSet::new();
        // Allow all priorities up to and including the max.
        if max_priority >= Priority::Bulk {
            allowed.insert(Priority::Bulk);
        }
        if max_priority >= Priority::Batch {
            allowed.insert(Priority::Batch);
        }
        if max_priority >= Priority::Interactive {
            allowed.insert(Priority::Interactive);
        }
        Self {
            max_priority,
            allowed,
        }
    }

    /// Create a cap from an explicit list of allowed priorities.
    #[must_use]
    pub fn from_allowed(priorities: &[Priority]) -> Self {
        let max_priority = priorities.iter().copied().max().unwrap_or(Priority::Bulk);
        Self {
            max_priority,
            allowed: priorities.iter().copied().collect(),
        }
    }

    /// Check whether a priority is allowed.
    pub fn check(&self, priority: Priority) -> Result<(), AdvisoryError> {
        if priority > self.max_priority {
            Err(AdvisoryError::PriorityNotAllowed)
        } else {
            Ok(())
        }
    }

    /// Narrow the allowed priorities (I-WA18).
    pub fn narrow(&mut self, new_allowed: &[Priority]) {
        self.allowed = new_allowed.iter().copied().collect();
        self.max_priority = new_allowed.iter().copied().max().unwrap_or(Priority::Bulk);
    }

    /// Check whether a priority is still in the allowed set.
    #[must_use]
    pub fn is_allowed(&self, priority: &Priority) -> bool {
        self.allowed.contains(priority)
    }
}

// =============================================================================
// Prefetch budget
// =============================================================================

/// Prefetch budget enforcement for a workload.
#[derive(Clone, Debug)]
pub struct PrefetchBudget {
    /// Maximum declared prefetch bytes.
    pub max_bytes: u64,
}

impl PrefetchBudget {
    /// Create a new prefetch budget.
    #[must_use]
    pub fn new(max_bytes: u64) -> Self {
        Self { max_bytes }
    }

    /// Cap a prefetch request to the budget, returning `(accepted_bytes, was_capped)`.
    #[must_use]
    pub fn cap(&self, requested_bytes: u64) -> (u64, bool) {
        if requested_bytes <= self.max_bytes {
            (requested_bytes, false)
        } else {
            (self.max_bytes, true)
        }
    }
}

// =============================================================================
// Scope validation
// =============================================================================

/// Workload-scoped ownership check for compositions.
pub struct ScopeValidator {
    /// Composition IDs owned by this workload.
    owned_compositions: HashSet<String>,
}

impl ScopeValidator {
    /// Create a scope validator with the given owned compositions.
    #[must_use]
    pub fn new(owned: &[&str]) -> Self {
        Self {
            owned_compositions: owned.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    /// Check whether a composition is owned by this workload.
    /// Returns `ScopeNotFound` for both unauthorized and non-existent
    /// compositions (I-WA6 indistinguishability).
    pub fn check(&self, composition_id: &str) -> Result<(), AdvisoryError> {
        if self.owned_compositions.contains(composition_id) {
            Ok(())
        } else {
            Err(AdvisoryError::ScopeNotFound)
        }
    }
}

// =============================================================================
// Forbidden target fields (I-WA11)
// =============================================================================

/// Fields that MUST NOT appear in advisory hint targets.
const FORBIDDEN_FIELDS: &[&str] = &[
    "shard_id",
    "log_position",
    "chunk_id",
    "dedup_hash",
    "node_id",
    "device_id",
];

/// Check whether a hint target field name is forbidden (I-WA11).
pub fn check_forbidden_target_field(field_name: &str) -> Result<(), AdvisoryError> {
    if FORBIDDEN_FIELDS.contains(&field_name) {
        Err(AdvisoryError::ForbiddenTargetField(field_name.to_owned()))
    } else {
        Ok(())
    }
}

// =============================================================================
// Advisory state FSM (I-WA12)
// =============================================================================

/// Advisory state for a workload (enabled / draining / disabled).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdvisoryState {
    /// Advisory is enabled, new workflows can be declared.
    Enabled,
    /// Draining: existing workflows continue, new declares rejected.
    Draining,
    /// Advisory is disabled, all declares rejected.
    Disabled,
}

impl AdvisoryState {
    /// Check whether a new workflow can be declared in this state.
    pub fn check_declare(&self) -> Result<(), AdvisoryError> {
        match self {
            Self::Enabled => Ok(()),
            Self::Draining | Self::Disabled => Err(AdvisoryError::AdvisoryDisabled),
        }
    }

    /// Check whether existing hints can continue in this state.
    #[must_use]
    pub fn allows_existing_hints(&self) -> bool {
        matches!(self, Self::Enabled | Self::Draining)
    }

    /// Transition from enabled to draining.
    pub fn start_draining(&mut self) -> bool {
        if *self == Self::Enabled {
            *self = Self::Draining;
            true
        } else {
            false
        }
    }

    /// Transition from draining to disabled (when all workflows ended).
    pub fn complete_drain(&mut self) -> bool {
        if *self == Self::Draining {
            *self = Self::Disabled;
            true
        } else {
            false
        }
    }
}

// =============================================================================
// Client ID registrar (I-WA10)
// =============================================================================

/// Registrar that tracks active client IDs and prevents re-registration.
pub struct ClientRegistrar {
    active: HashSet<ClientId>,
}

impl ClientRegistrar {
    /// Create a new empty registrar.
    #[must_use]
    pub fn new() -> Self {
        Self {
            active: HashSet::new(),
        }
    }

    /// Register a new client ID. Returns `Err` if already registered.
    pub fn register(&mut self, client_id: ClientId) -> Result<(), AdvisoryError> {
        if self.active.contains(&client_id) {
            Err(AdvisoryError::ScopeNotFound)
        } else {
            self.active.insert(client_id);
            Ok(())
        }
    }

    /// Deregister a client ID (e.g., on TTL expiry).
    pub fn deregister(&mut self, client_id: &ClientId) {
        self.active.remove(client_id);
    }
}

impl Default for ClientRegistrar {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// Workload policy (composite)
// =============================================================================

/// Composite workload-level advisory policy.
#[derive(Clone, Debug)]
pub struct WorkloadPolicy {
    /// Allowed profiles.
    pub profile_allow_list: ProfileAllowList,
    /// Priority cap.
    pub priority_cap: PriorityCap,
    /// Prefetch budget.
    pub prefetch_budget: PrefetchBudget,
    /// Advisory state.
    pub state: AdvisoryState,
    /// Authorized pool descriptors.
    pub authorized_pools: Vec<PoolDescriptor>,
}

impl WorkloadPolicy {
    /// Create a new policy.
    #[must_use]
    pub fn new(
        profiles: &[WorkloadProfile],
        max_priority: Priority,
        prefetch_bytes: u64,
        pools: Vec<PoolDescriptor>,
    ) -> Self {
        Self {
            profile_allow_list: ProfileAllowList::new(profiles),
            priority_cap: PriorityCap::new(max_priority),
            prefetch_budget: PrefetchBudget::new(prefetch_bytes),
            state: AdvisoryState::Enabled,
            authorized_pools: pools,
        }
    }

    /// Validate a pool handle against the authorized set.
    pub fn check_pool_handle(&self, handle: &PoolHandle) -> Result<(), AdvisoryError> {
        if self.authorized_pools.iter().any(|d| d.handle == *handle) {
            Ok(())
        } else {
            Err(AdvisoryError::ScopeNotFound)
        }
    }
}

// =============================================================================
// Deadline hint
// =============================================================================

/// A deadline hint (best-effort scheduling guidance, I-WA1).
#[derive(Clone, Debug)]
pub struct DeadlineHint {
    /// Composition target.
    pub composition_id: String,
    /// Deadline timestamp (epoch seconds).
    pub deadline_epoch_secs: u64,
}

impl DeadlineHint {
    /// Validate a deadline hint. Past deadlines are rejected.
    pub fn validate(&self, now_epoch_secs: u64) -> Result<(), AdvisoryError> {
        if self.deadline_epoch_secs <= now_epoch_secs {
            Err(AdvisoryError::ForbiddenTargetField(
                "deadline in the past".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

// =============================================================================
// Collective announcement
// =============================================================================

/// Collective checkpoint announcement (advisory, best-effort warm-up).
#[derive(Clone, Debug)]
pub struct CollectiveAnnouncement {
    /// Number of MPI ranks.
    pub ranks: u32,
    /// Bytes per rank.
    pub bytes_per_rank: u64,
    /// Deadline (epoch seconds).
    pub deadline_epoch_secs: u64,
}

impl CollectiveAnnouncement {
    /// Announcements are always accepted (advisory only). Returns true.
    #[must_use]
    pub fn is_advisory_only(&self) -> bool {
        true
    }

    /// Total bytes.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        u64::from(self.ranks) * self.bytes_per_rank
    }
}
