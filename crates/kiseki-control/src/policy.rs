//! Quota enforcement and compliance tag evaluation.
//!
//! Spec: I-T2 (quota enforcement), I-K9 (compliance floor on staleness).

use kiseki_common::tenancy::ComplianceTag;

/// Compute effective staleness bound for a view.
///
/// The effective bound is `max(view_preference, compliance_floor)` —
/// compliance tags set a non-overridable floor (I-K9).
#[must_use]
pub fn effective_staleness(tags: &[ComplianceTag], view_preference_ms: u64) -> u64 {
    let floor = compliance_floor_ms(tags);
    view_preference_ms.max(floor)
}

/// Minimum staleness bound implied by compliance tags.
/// HIPAA has a 2-second floor.
fn compliance_floor_ms(tags: &[ComplianceTag]) -> u64 {
    let mut floor = 0u64;
    for tag in tags {
        if let ComplianceTag::Hipaa = tag {
            floor = floor.max(2000);
        }
    }
    floor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hipaa_floor() {
        let tags = vec![ComplianceTag::Hipaa, ComplianceTag::Gdpr];
        assert_eq!(effective_staleness(&tags, 1000), 2000);
        assert_eq!(effective_staleness(&tags, 5000), 5000);
    }

    #[test]
    fn no_tags() {
        assert_eq!(effective_staleness(&[], 500), 500);
    }
}
