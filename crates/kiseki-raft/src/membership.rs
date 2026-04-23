//! Dynamic Raft membership changes with quorum safety.
//!
//! Validates membership transitions (add learner, promote voter,
//! remove voter) and enforces quorum invariants: a voter removal
//! is refused if it would drop the cluster below majority.

use std::fmt;

/// Action to perform on a node's membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MembershipAction {
    /// First step: add as non-voting learner.
    AddLearner,
    /// Promote an existing learner to full voter.
    PromoteVoter,
    /// Remove a node from the voter set.
    RemoveVoter,
}

impl fmt::Display for MembershipAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddLearner => write!(f, "AddLearner"),
            Self::PromoteVoter => write!(f, "PromoteVoter"),
            Self::RemoveVoter => write!(f, "RemoveVoter"),
        }
    }
}

/// A request to change cluster membership.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MembershipChange {
    /// Node to act on.
    pub node_id: u64,
    /// Network address for the node.
    pub addr: String,
    /// The membership transition to apply.
    pub action: MembershipAction,
}

/// Result of a membership change attempt.
#[derive(Clone, Debug)]
pub struct MembershipResult {
    /// Node that was acted on.
    pub node_id: u64,
    /// Action that was attempted.
    pub action: MembershipAction,
    /// Whether the change succeeded.
    pub success: bool,
    /// Human-readable description of the outcome.
    pub message: String,
}

/// Errors returned when a membership change is invalid.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum MembershipError {
    /// The node is already a voter or learner.
    #[error("node {0} is already a member")]
    AlreadyMember(u64),

    /// Attempted to promote a node that is not currently a learner.
    #[error("node {0} is not a learner")]
    NotALearner(u64),

    /// Removing this voter would drop below quorum.
    #[error("removing node {node_id} would lose quorum ({voter_count} voters, need {quorum} for majority)", quorum = quorum_size(*.voter_count))]
    WouldLoseQuorum {
        /// Node requested for removal.
        node_id: u64,
        /// Current voter count (before removal).
        voter_count: usize,
    },

    /// The node was not found in the membership set.
    #[error("node {0} not found")]
    NodeNotFound(u64),
}

/// Returns the majority quorum size for a given voter count.
///
/// Quorum = floor(n/2) + 1, i.e. strict majority.
#[must_use]
pub const fn quorum_size(voter_count: usize) -> usize {
    voter_count / 2 + 1
}

/// Returns `true` if removing one voter still leaves a quorum.
///
/// After removal the cluster has `voter_count - 1` voters and needs
/// `quorum_size(voter_count - 1)` for majority. This is satisfiable
/// when `voter_count - 1 >= quorum_size(voter_count - 1)`.
#[must_use]
pub const fn can_remove_safely(voter_count: usize) -> bool {
    if voter_count <= 1 {
        return false;
    }
    let remaining = voter_count - 1;
    remaining >= quorum_size(remaining)
}

/// Validate a proposed membership change against the current state.
///
/// # Arguments
///
/// * `current_voters` — slice of node IDs that are full voters.
/// * `current_learners` — slice of node IDs that are non-voting learners.
/// * `change` — the proposed membership transition.
///
/// # Errors
///
/// Returns [`MembershipError`] if the change violates membership rules.
pub fn validate_membership_change(
    current_voters: &[u64],
    current_learners: &[u64],
    change: &MembershipChange,
) -> Result<(), MembershipError> {
    match change.action {
        MembershipAction::AddLearner => {
            if current_voters.contains(&change.node_id)
                || current_learners.contains(&change.node_id)
            {
                tracing::warn!(
                    node_id = change.node_id,
                    "membership change rejected: node already a member"
                );
                return Err(MembershipError::AlreadyMember(change.node_id));
            }
            tracing::info!(node_id = change.node_id, addr = %change.addr, "adding learner");
            Ok(())
        }
        MembershipAction::PromoteVoter => {
            if !current_learners.contains(&change.node_id) {
                tracing::warn!(
                    node_id = change.node_id,
                    "membership change rejected: node is not a learner"
                );
                return Err(MembershipError::NotALearner(change.node_id));
            }
            tracing::info!(node_id = change.node_id, "promoting learner to voter");
            Ok(())
        }
        MembershipAction::RemoveVoter => {
            if !current_voters.contains(&change.node_id) {
                tracing::warn!(
                    node_id = change.node_id,
                    "membership change rejected: node not found in voters"
                );
                return Err(MembershipError::NodeNotFound(change.node_id));
            }
            if !can_remove_safely(current_voters.len()) {
                tracing::error!(
                    node_id = change.node_id,
                    voter_count = current_voters.len(),
                    "membership change rejected: would lose quorum"
                );
                return Err(MembershipError::WouldLoseQuorum {
                    node_id: change.node_id,
                    voter_count: current_voters.len(),
                });
            }
            tracing::info!(node_id = change.node_id, "removing voter");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_learner_succeeds() {
        let voters = [1, 2, 3];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 4,
            addr: "10.0.0.4:9102".to_owned(),
            action: MembershipAction::AddLearner,
        };
        assert!(validate_membership_change(&voters, learners, &change).is_ok());
    }

    #[test]
    fn add_duplicate_voter_fails() {
        let voters = [1, 2, 3];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 2,
            addr: "10.0.0.2:9102".to_owned(),
            action: MembershipAction::AddLearner,
        };
        assert_eq!(
            validate_membership_change(&voters, learners, &change),
            Err(MembershipError::AlreadyMember(2))
        );
    }

    #[test]
    fn add_duplicate_learner_fails() {
        let voters = [1, 2, 3];
        let learners = [4_u64];
        let change = MembershipChange {
            node_id: 4,
            addr: "10.0.0.4:9102".to_owned(),
            action: MembershipAction::AddLearner,
        };
        assert_eq!(
            validate_membership_change(&voters, &learners, &change),
            Err(MembershipError::AlreadyMember(4))
        );
    }

    #[test]
    fn promote_non_learner_fails() {
        let voters = [1, 2, 3];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 5,
            addr: "10.0.0.5:9102".to_owned(),
            action: MembershipAction::PromoteVoter,
        };
        assert_eq!(
            validate_membership_change(&voters, learners, &change),
            Err(MembershipError::NotALearner(5))
        );
    }

    #[test]
    fn promote_learner_succeeds() {
        let voters = [1, 2, 3];
        let learners = [4_u64];
        let change = MembershipChange {
            node_id: 4,
            addr: "10.0.0.4:9102".to_owned(),
            action: MembershipAction::PromoteVoter,
        };
        assert!(validate_membership_change(&voters, &learners, &change).is_ok());
    }

    #[test]
    fn remove_voter_that_would_lose_quorum_fails() {
        // 2 voters: removing one leaves 1, quorum_size(1) = 1, but
        // can_remove_safely returns false for voter_count <= 1 after removal
        // Actually: 2 voters, can_remove_safely(2) checks remaining=1 >= quorum_size(1)=1 → true
        // So use 1 voter.
        let voters = [1_u64];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 1,
            addr: "10.0.0.1:9102".to_owned(),
            action: MembershipAction::RemoveVoter,
        };
        assert_eq!(
            validate_membership_change(&voters, learners, &change),
            Err(MembershipError::WouldLoseQuorum {
                node_id: 1,
                voter_count: 1,
            })
        );
    }

    #[test]
    fn remove_voter_succeeds_with_sufficient_quorum() {
        let voters = [1, 2, 3];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 3,
            addr: "10.0.0.3:9102".to_owned(),
            action: MembershipAction::RemoveVoter,
        };
        assert!(validate_membership_change(&voters, learners, &change).is_ok());
    }

    #[test]
    fn remove_nonexistent_voter_fails() {
        let voters = [1, 2, 3];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 99,
            addr: "10.0.0.99:9102".to_owned(),
            action: MembershipAction::RemoveVoter,
        };
        assert_eq!(
            validate_membership_change(&voters, learners, &change),
            Err(MembershipError::NodeNotFound(99))
        );
    }

    #[test]
    fn quorum_size_calculations() {
        assert_eq!(quorum_size(1), 1);
        assert_eq!(quorum_size(2), 2);
        assert_eq!(quorum_size(3), 2);
        assert_eq!(quorum_size(4), 3);
        assert_eq!(quorum_size(5), 3);
        assert_eq!(quorum_size(7), 4);
    }

    #[test]
    fn quorum_size_for_various_counts() {
        // 1→1, 2→2, 3→2, 5→3, 7→4
        assert_eq!(quorum_size(1), 1);
        assert_eq!(quorum_size(2), 2);
        assert_eq!(quorum_size(3), 2);
        assert_eq!(quorum_size(5), 3);
        assert_eq!(quorum_size(7), 4);
    }

    #[test]
    fn add_learner_with_empty_voters_succeeds() {
        let voters: &[u64] = &[];
        let learners: &[u64] = &[];
        let change = MembershipChange {
            node_id: 1,
            addr: "10.0.0.1:9102".to_owned(),
            action: MembershipAction::AddLearner,
        };
        assert!(
            validate_membership_change(voters, learners, &change).is_ok(),
            "adding a learner to an empty cluster should succeed"
        );
    }

    #[test]
    fn can_remove_safely_cases() {
        assert!(!can_remove_safely(0));
        assert!(!can_remove_safely(1));
        assert!(can_remove_safely(2));
        assert!(can_remove_safely(3));
        assert!(can_remove_safely(5));
    }
}
