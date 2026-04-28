//! Cluster-fabric SAN role check.
//!
//! Phase 16a — D-1 / proto comment. Only intra-cluster mTLS certs
//! with the `spiffe://cluster/fabric/<node-id>` SAN URI are accepted
//! on the [`ClusterChunkService`][cs] endpoint. Tenant clients (which
//! carry `spiffe://cluster/org/<uuid>` per `kiseki-transport`) are
//! rejected with `PermissionDenied` so a leaked tenant cert cannot
//! exfiltrate fragments via the cross-node fabric.
//!
//! [cs]: kiseki_proto::v1::cluster_chunk_service_server::ClusterChunkService

use thiserror::Error;
use x509_parser::extensions::{GeneralName, ParsedExtension};
use x509_parser::oid_registry::OID_X509_EXT_SUBJECT_ALT_NAME;
use x509_parser::prelude::FromDer;

const FABRIC_SAN_PREFIX: &str = "spiffe://cluster/fabric/";

/// Errors from the SAN-role check. Map to gRPC `Status::permission_denied`
/// at the interceptor boundary.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum FabricAuthError {
    /// Cert DER could not be parsed.
    #[error("invalid certificate: {0}")]
    InvalidCert(String),
    /// Cert has no Subject Alternative Name extension.
    #[error("certificate has no SAN extension")]
    MissingSan,
    /// SAN does not contain a `spiffe://cluster/fabric/<node>` URI.
    /// The cert is well-formed but is not a fabric-role cert (most
    /// likely a tenant cert presented to the fabric port — rejected
    /// by I-Auth4 / I-T1).
    #[error("certificate is not a fabric-role cert")]
    NotFabricRole,
    /// SAN URI is fabric-role but the node-id segment is empty.
    #[error("fabric SAN has empty node id")]
    EmptyNodeId,
}

/// Parse a peer's leaf certificate (DER) and confirm it carries a
/// `spiffe://cluster/fabric/<node-id>` SAN URI. On success returns the
/// node id (the URI suffix), used for logging / metrics.
pub fn verify_fabric_san(cert_der: &[u8]) -> Result<String, FabricAuthError> {
    let (_, cert) = x509_parser::certificate::X509Certificate::from_der(cert_der)
        .map_err(|e| FabricAuthError::InvalidCert(format!("X.509 parse failed: {e}")))?;

    let san_ext = cert
        .extensions()
        .iter()
        .find(|ext| ext.oid == OID_X509_EXT_SUBJECT_ALT_NAME)
        .ok_or(FabricAuthError::MissingSan)?;

    let parsed = san_ext.parsed_extension();
    let ParsedExtension::SubjectAlternativeName(san) = parsed else {
        return Err(FabricAuthError::MissingSan);
    };

    for name in &san.general_names {
        if let GeneralName::URI(uri) = name {
            if let Some(node_id) = uri.strip_prefix(FABRIC_SAN_PREFIX) {
                if node_id.is_empty() {
                    return Err(FabricAuthError::EmptyNodeId);
                }
                return Ok(node_id.to_owned());
            }
        }
    }

    Err(FabricAuthError::NotFabricRole)
}

#[cfg(test)]
mod tests {
    use rcgen::{CertificateParams, KeyPair, SanType};

    use super::*;

    fn cert_with_sans(sans: Vec<SanType>) -> Vec<u8> {
        let key = KeyPair::generate().expect("keypair");
        let mut params = CertificateParams::new(vec![]).expect("params");
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "test-cert");
        params.subject_alt_names = sans;
        params
            .self_signed(&key)
            .expect("self-sign")
            .der()
            .to_vec()
    }

    #[test]
    fn fabric_role_san_extracts_node_id() {
        let der = cert_with_sans(vec![SanType::URI(
            "spiffe://cluster/fabric/node-1".try_into().unwrap(),
        )]);
        let node_id = verify_fabric_san(&der).expect("fabric SAN accepted");
        assert_eq!(node_id, "node-1");
    }

    #[test]
    fn tenant_san_rejected_as_not_fabric_role() {
        // Mirrors what kiseki-transport issues for tenant data-fabric.
        let der = cert_with_sans(vec![SanType::URI(
            "spiffe://cluster/org/00000000-0000-0000-0000-000000000001"
                .try_into()
                .unwrap(),
        )]);
        let err = verify_fabric_san(&der).expect_err("tenant SAN must be rejected");
        assert_eq!(err, FabricAuthError::NotFabricRole);
    }

    #[test]
    fn cert_with_no_san_rejected() {
        // Cert with no SANs at all.
        let der = cert_with_sans(vec![]);
        let err = verify_fabric_san(&der).expect_err("must reject");
        // Either MissingSan or NotFabricRole depending on rcgen
        // (rcgen omits the extension entirely when sans is empty).
        assert!(
            matches!(err, FabricAuthError::MissingSan | FabricAuthError::NotFabricRole),
            "got {err:?}"
        );
    }

    #[test]
    fn invalid_cert_bytes_rejected() {
        let err = verify_fabric_san(b"not a der cert").expect_err("garbage");
        assert!(matches!(err, FabricAuthError::InvalidCert(_)));
    }

    #[test]
    fn fabric_san_with_empty_node_id_rejected() {
        let der = cert_with_sans(vec![SanType::URI(
            "spiffe://cluster/fabric/".try_into().unwrap(),
        )]);
        let err = verify_fabric_san(&der).expect_err("empty node id");
        assert_eq!(err, FabricAuthError::EmptyNodeId);
    }
}
