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
