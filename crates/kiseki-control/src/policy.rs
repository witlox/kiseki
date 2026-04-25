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

/// Check whether compression can be enabled for an org with the given compliance tags.
///
/// HIPAA-tagged orgs cannot enable compression (data-at-rest integrity requirement).
pub fn enable_compression(tags: &[ComplianceTag]) -> Result<(), crate::error::ControlError> {
    for tag in tags {
        if let ComplianceTag::Hipaa = tag {
            return Err(crate::error::ControlError::Rejected(
                "compression cannot be enabled for HIPAA-tagged organizations".into(),
            ));
        }
    }
    Ok(())
}

/// Validate a crypto-shred cache TTL value.
///
/// Acceptable range is [5, 300] seconds. Values outside this range are rejected.
pub fn validate_cache_ttl(secs: u64) -> Result<u64, crate::error::ControlError> {
    if secs < 5 {
        return Err(crate::error::ControlError::Rejected(
            format!("cache TTL {secs}s below minimum of 5s"),
        ));
    }
    if secs > 300 {
        return Err(crate::error::ControlError::Rejected(
            format!("cache TTL {secs}s exceeds maximum of 300s"),
        ));
    }
    Ok(secs)
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

    #[test]
    fn hipaa_blocks_compression_opt_in() {
        let tags = vec![ComplianceTag::Hipaa];
        let result = enable_compression(&tags);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("HIPAA"), "error should mention HIPAA: {err}");
    }

    #[test]
    fn non_hipaa_allows_compression() {
        let tags = vec![ComplianceTag::Gdpr, ComplianceTag::RevFadp];
        assert!(enable_compression(&tags).is_ok());
    }

    #[test]
    fn validate_cache_ttl_in_range() {
        assert_eq!(validate_cache_ttl(5).unwrap(), 5);
        assert_eq!(validate_cache_ttl(60).unwrap(), 60);
        assert_eq!(validate_cache_ttl(300).unwrap(), 300);
    }

    #[test]
    fn validate_cache_ttl_below_minimum_rejected() {
        let result = validate_cache_ttl(2);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("minimum"), "error should mention minimum: {err}");
    }

    #[test]
    fn validate_cache_ttl_above_maximum_rejected() {
        let result = validate_cache_ttl(500);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("maximum"), "error should mention maximum: {err}");
    }
}
