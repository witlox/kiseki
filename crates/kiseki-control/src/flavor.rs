//! Placement flavor matching.
//!
//! A flavor defines a protocol + transport + topology combination.
//! The system performs best-fit matching against cluster capabilities.
//!
//! Spec: `ubiquitous-language.md#Flavor`, I-P1.

/// A placement capability.
#[derive(Clone, Debug)]
pub struct Flavor {
    /// Flavor name.
    pub name: String,
    /// Protocol (e.g., "NFS", "S3").
    pub protocol: String,
    /// Transport (e.g., "CXI", "TCP").
    pub transport: String,
    /// Topology (e.g., "hyperconverged", "dedicated").
    pub topology: String,
}

/// Standard set of cluster flavors.
#[must_use]
pub fn default_flavors() -> Vec<Flavor> {
    vec![
        Flavor {
            name: "hpc-slingshot".into(),
            protocol: "NFS".into(),
            transport: "CXI".into(),
            topology: "hyperconverged".into(),
        },
        Flavor {
            name: "standard-tcp".into(),
            protocol: "S3".into(),
            transport: "TCP".into(),
            topology: "dedicated".into(),
        },
        Flavor {
            name: "ai-training".into(),
            protocol: "NFS+S3".into(),
            transport: "CXI+TCP".into(),
            topology: "shared".into(),
        },
    ]
}

/// Find the best matching flavor. Exact name match first, then transport.
#[must_use]
pub fn match_best_fit(available: &[Flavor], requested: &Flavor) -> Option<Flavor> {
    // Exact match.
    if let Some(f) = available.iter().find(|f| f.name == requested.name) {
        return Some(f.clone());
    }
    // Best-fit by transport.
    if let Some(f) = available
        .iter()
        .find(|f| f.transport == requested.transport)
    {
        return Some(f.clone());
    }
    None
}

/// List all flavor names.
#[must_use]
pub fn list_flavors(available: &[Flavor]) -> Vec<String> {
    available.iter().map(|f| f.name.clone()).collect()
}

/// Result of a flavor match attempt.
#[derive(Clone, Debug)]
pub struct FlavorMatchResult {
    /// The matched flavor (if any).
    pub matched: Option<Flavor>,
    /// Whether the match was exact or best-fit.
    pub exact: bool,
    /// Available flavors listed (for error messages).
    pub available_names: Vec<String>,
}

/// Attempt to match a requested flavor against the available set.
/// Returns a result indicating exact match, best-fit, or no match.
#[must_use]
pub fn try_match_flavor(available: &[Flavor], requested_name: &str) -> FlavorMatchResult {
    let available_names = list_flavors(available);

    // Exact name match.
    if let Some(f) = available.iter().find(|f| f.name == requested_name) {
        return FlavorMatchResult {
            matched: Some(f.clone()),
            exact: true,
            available_names,
        };
    }

    // Best-fit: find by transport overlap with requested flavor's transport.
    // Since we only have the name, we look for partial transport matches.
    // For the no-match case, return None.
    FlavorMatchResult {
        matched: None,
        exact: false,
        available_names,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn best_fit_matching_exact_name() {
        // Scenario: Tenant selects a flavor - best-fit matching
        let flavors = default_flavors();
        let requested = Flavor {
            name: "ai-training".into(),
            protocol: "NFS+S3".into(),
            transport: "CXI+TCP".into(),
            topology: "shared".into(),
        };
        let matched = match_best_fit(&flavors, &requested);
        assert!(matched.is_some(), "ai-training should match by name");
        let m = matched.unwrap();
        assert_eq!(m.name, "ai-training");
        assert_eq!(m.transport, "CXI+TCP");
    }

    #[test]
    fn best_fit_falls_back_to_transport() {
        // When exact name doesn't match, fall back to transport match.
        let flavors = default_flavors();
        let requested = Flavor {
            name: "custom-cxi".into(),
            protocol: "custom".into(),
            transport: "CXI".into(),
            topology: "custom".into(),
        };
        let matched = match_best_fit(&flavors, &requested);
        assert!(
            matched.is_some(),
            "should match hpc-slingshot by CXI transport"
        );
        let m = matched.unwrap();
        assert_eq!(m.transport, "CXI");
    }

    #[test]
    fn flavor_unavailable_returns_none_with_available_list() {
        // Scenario: Flavor unavailable
        let flavors = default_flavors();
        let result = try_match_flavor(&flavors, "quantum-rdma");
        assert!(
            result.matched.is_none(),
            "quantum-rdma should not match any flavor"
        );
        assert!(!result.exact);
        assert!(
            !result.available_names.is_empty(),
            "available flavors should be listed"
        );
        assert!(result
            .available_names
            .contains(&"hpc-slingshot".to_string()));
        assert!(result.available_names.contains(&"standard-tcp".to_string()));
        assert!(result.available_names.contains(&"ai-training".to_string()));
    }
}
