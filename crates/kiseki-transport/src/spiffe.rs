//! SPIFFE SVID identity extraction (I-Auth1).
//!
//! Parses SPIFFE URIs from X.509 certificates presented during mTLS.
//! Format: `spiffe://<trust-domain>/<workload-path>`

/// A parsed SPIFFE SVID URI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiffeId {
    /// Trust domain (e.g., "kiseki.prod").
    pub trust_domain: String,
    /// Workload path segments (e.g., `["ns", "default", "sa", "data-gateway"]`).
    pub path: Vec<String>,
}

impl SpiffeId {
    /// Parse a SPIFFE URI. Returns `None` if the format is invalid.
    #[must_use]
    pub fn parse(uri: &str) -> Option<Self> {
        let rest = uri.strip_prefix("spiffe://")?;
        let (domain, path_str) = rest.split_once('/')?;

        if domain.is_empty() {
            return None;
        }

        let path: Vec<String> = path_str
            .split('/')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();

        Some(Self {
            trust_domain: domain.to_owned(),
            path,
        })
    }

    /// Full URI representation.
    #[must_use]
    pub fn uri(&self) -> String {
        format!("spiffe://{}/{}", self.trust_domain, self.path.join("/"))
    }

    /// Check if this ID belongs to a given trust domain.
    #[must_use]
    pub fn in_domain(&self, domain: &str) -> bool {
        self.trust_domain == domain
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_spiffe() {
        let id = SpiffeId::parse("spiffe://kiseki.prod/ns/default/sa/gateway").unwrap();
        assert_eq!(id.trust_domain, "kiseki.prod");
        assert_eq!(id.path, vec!["ns", "default", "sa", "gateway"]);
    }

    #[test]
    fn parse_minimal_path() {
        let id = SpiffeId::parse("spiffe://example.com/workload").unwrap();
        assert_eq!(id.trust_domain, "example.com");
        assert_eq!(id.path, vec!["workload"]);
    }

    #[test]
    fn roundtrip() {
        let uri = "spiffe://kiseki.prod/ns/default/sa/gateway";
        let id = SpiffeId::parse(uri).unwrap();
        assert_eq!(id.uri(), uri);
    }

    #[test]
    fn invalid_no_scheme() {
        assert!(SpiffeId::parse("https://example.com/path").is_none());
    }

    #[test]
    fn invalid_empty_domain() {
        assert!(SpiffeId::parse("spiffe:///path").is_none());
    }

    #[test]
    fn domain_check() {
        let id = SpiffeId::parse("spiffe://kiseki.prod/svc").unwrap();
        assert!(id.in_domain("kiseki.prod"));
        assert!(!id.in_domain("other.domain"));
    }
}
