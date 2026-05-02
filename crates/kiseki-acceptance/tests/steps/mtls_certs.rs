//! In-process cert generation for the @mtls cluster harness.
//!
//! Used by the multi-node-raft "Tenant cert presented to fabric
//! port is rejected (I-Auth4)" scenario. Pattern lifted from
//! `kiseki-transport/tests/tls_handshake.rs`. Cert layout:
//!
//! - **CA** — self-signed root, sized for ECDSA P-256 (matches the
//!   docker-compose `gen-tls-certs.sh` material so the runtime's
//!   `build_tls()` accepts both).
//! - **Per-node fabric cert** — signed by the CA, carries the SAN URI
//!   `spiffe://cluster/fabric/node-{id}` plus DNS `localhost` and IP
//!   `127.0.0.1` so the data-path mTLS handshake completes regardless
//!   of how the peer addresses the node.
//! - **Tenant cert** — signed by the same CA, SAN URI
//!   `spiffe://cluster/org/<uuid>`. Used by the negative scenario to
//!   prove the SAN-role interceptor rejects non-fabric callers with
//!   `PermissionDenied`.
//!
//! Certs are written into a tempdir owned by the harness; the `Drop`
//! impl deletes them. Production reads cert paths from
//! `KISEKI_CA_PATH` / `KISEKI_CERT_PATH` / `KISEKI_KEY_PATH` env vars
//! — the harness sets exactly those.

use std::path::{Path, PathBuf};

use rcgen::{CertificateParams, Issuer, KeyPair};
use tempfile::TempDir;

/// Paths to the generated cert material on disk. Lifetime tied to
/// the owning `TempDir` (in `MtlsCerts`).
pub struct NodeCertPaths {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// One CA + N per-node fabric certs + 1 tenant cert. Generated once
/// per harness; the `TempDir` keeps the files alive for the harness's
/// lifetime.
pub struct MtlsCerts {
    _dir: TempDir,
    ca_pem_path: PathBuf,
    /// `node_id` → (cert_pem_path, key_pem_path)
    nodes: std::collections::BTreeMap<u64, NodeCertPaths>,
    tenant_cert_path: PathBuf,
    tenant_key_path: PathBuf,
}

impl MtlsCerts {
    /// Generate a CA + per-node fabric certs for `node_ids` + a single
    /// tenant cert. The tenant cert's SAN URI is keyed off a fixed
    /// UUID so the assertion side (`spiffe://cluster/org/<uuid>`) can
    /// be specific.
    pub fn generate(node_ids: &[u64]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir for mtls certs");
        let dir_path = dir.path().to_path_buf();

        // 1. CA.
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "kiseki-test-ca");
        ca_params
            .distinguished_name
            .push(rcgen::DnType::OrganizationName, "kiseki-test");
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.clone().self_signed(&ca_key).unwrap();
        let ca_pem_path = dir_path.join("ca.pem");
        std::fs::write(&ca_pem_path, ca_cert.pem()).unwrap();
        let issuer = Issuer::new(ca_params, ca_key);

        // 2. Per-node fabric certs.
        let mut nodes = std::collections::BTreeMap::new();
        for &id in node_ids {
            let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::NoCa;
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, format!("kiseki-node-{id}"));
            params
                .subject_alt_names
                .push(rcgen::SanType::DnsName("localhost".try_into().unwrap()));
            params
                .subject_alt_names
                .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
            // The SAN URI the fabric SAN interceptor checks for.
            params.subject_alt_names.push(rcgen::SanType::URI(
                format!("spiffe://cluster/fabric/node-{id}")
                    .try_into()
                    .unwrap(),
            ));
            let key = KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &issuer).unwrap();
            let cert_path = dir_path.join(format!("node-{id}.pem"));
            let key_path = dir_path.join(format!("node-{id}.key"));
            std::fs::write(&cert_path, cert.pem()).unwrap();
            std::fs::write(&key_path, key.serialize_pem()).unwrap();
            nodes.insert(
                id,
                NodeCertPaths {
                    ca: ca_pem_path.clone(),
                    cert: cert_path,
                    key: key_path,
                },
            );
        }

        // 3. Tenant cert — wrong SAN role; the SAN interceptor must
        //    reject this. Use a fixed UUID so callers can include it
        //    in error-message assertions if they want.
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::NoCa;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "kiseki-tenant-test");
        params
            .subject_alt_names
            .push(rcgen::SanType::DnsName("localhost".try_into().unwrap()));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
        params.subject_alt_names.push(rcgen::SanType::URI(
            "spiffe://cluster/org/00000000-0000-0000-0000-000000000042"
                .try_into()
                .unwrap(),
        ));
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &issuer).unwrap();
        let tenant_cert_path = dir_path.join("tenant.pem");
        let tenant_key_path = dir_path.join("tenant.key");
        std::fs::write(&tenant_cert_path, cert.pem()).unwrap();
        std::fs::write(&tenant_key_path, key.serialize_pem()).unwrap();

        Self {
            _dir: dir,
            ca_pem_path,
            nodes,
            tenant_cert_path,
            tenant_key_path,
        }
    }

    pub fn node(&self, id: u64) -> &NodeCertPaths {
        self.nodes
            .get(&id)
            .unwrap_or_else(|| panic!("no fabric cert for node-{id}"))
    }

    pub fn ca_path(&self) -> &Path {
        &self.ca_pem_path
    }

    pub fn tenant_cert_path(&self) -> &Path {
        &self.tenant_cert_path
    }

    pub fn tenant_key_path(&self) -> &Path {
        &self.tenant_key_path
    }
}
