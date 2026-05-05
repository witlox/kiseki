#![allow(clippy::unwrap_used, clippy::expect_used)]
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
use std::sync::Mutex;

use rcgen::{CertificateParams, Issuer, KeyPair};
use tempfile::TempDir;

/// Paths to the generated cert material on disk. Lifetime tied to
/// the owning `TempDir` (in `MtlsCerts`).
pub struct NodeCertPaths {
    pub ca: PathBuf,
    pub cert: PathBuf,
    pub key: PathBuf,
}

/// In-memory PEMs for one tenant's mTLS material. Ownership stays in
/// the `MtlsCerts` struct; callers borrow `&[u8]` slices to feed into
/// rustls / tonic configuration.
pub struct TenantClientCert {
    /// SAN URI carried by this cert (e.g. `spiffe://kiseki/tenant/org-pharma`).
    pub san_uri: String,
    /// Cert PEM, signed by the harness CA.
    pub cert_pem: String,
    /// Key PEM (private). Held in process memory, never on disk —
    /// scenarios mint fresh tenants frequently and we want each
    /// drop-of-the-harness to wipe everything.
    pub key_pem: String,
}

/// One CA + N per-node fabric certs + 1 (legacy) cluster-tenant cert
/// + a dynamic registry of kiseki-tenant client certs minted on
/// demand by the native-gateway BDD scenarios. Generated once per
/// harness; the `TempDir` keeps the on-disk files alive for the
/// harness's lifetime.
pub struct MtlsCerts {
    _dir: TempDir,
    ca_pem_path: PathBuf,
    ca_pem_text: String,
    /// `node_id` → (cert_pem_path, key_pem_path)
    nodes: std::collections::BTreeMap<u64, NodeCertPaths>,
    tenant_cert_path: PathBuf,
    tenant_key_path: PathBuf,
    /// CA issuer kept around so additional kiseki-tenant certs can be
    /// minted at scenario time without re-deriving the chain.
    ca_issuer: Mutex<Issuer<'static, KeyPair>>,
    /// Registry of `tenant_id → TenantClientCert`. Filled by
    /// `mint_kiseki_tenant_cert(tenant_id)`.
    kiseki_tenant_certs: Mutex<std::collections::HashMap<String, TenantClientCert>>,
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
        let ca_pem_text = ca_cert.pem();
        std::fs::write(&ca_pem_path, &ca_pem_text).unwrap();
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
            ca_pem_text,
            nodes,
            tenant_cert_path,
            tenant_key_path,
            ca_issuer: Mutex::new(issuer),
            kiseki_tenant_certs: Mutex::new(std::collections::HashMap::new()),
        }
    }

    /// CA PEM text — used to configure rustls clients with this CA as
    /// the only trust anchor (the harness's CA isn't in the system root
    /// store).
    #[must_use]
    pub fn ca_pem_text(&self) -> &str {
        &self.ca_pem_text
    }

    /// Mint (or reuse) a kiseki-tenant client cert. Idempotent on
    /// `tenant_id` — the second call for the same id returns the same
    /// cert. The cert is signed by the harness CA and carries the SAN
    /// URI `spiffe://kiseki/tenant/<tenant_id>` PLUS the standard
    /// `localhost` / `127.0.0.1` DNS/IP SANs so the rustls handshake
    /// doesn't reject on the server-name comparison.
    ///
    /// # Panics
    /// Panics if `tenant_id` doesn't satisfy the
    /// `kiseki-gateway::native::canonical_san` rules (lowercase ASCII,
    /// no slashes / percent-encoded unreserved bytes). The native
    /// gateway's interceptor would reject such a cert anyway, so
    /// failing fast here keeps test bugs from masking server bugs.
    pub fn mint_kiseki_tenant_cert(&self, tenant_id: &str) -> TenantClientCert {
        let san_uri = format!("spiffe://kiseki/tenant/{tenant_id}");
        // Sanity-check via the gateway's canonicalizer so a typo in
        // the test fixture surfaces as a panic, not a runtime
        // canonicalization-mismatch reject from the server.
        kiseki_gateway::native::canonical_san::canonicalize(&san_uri)
            .unwrap_or_else(|e| {
                panic!("non-canonical kiseki tenant SAN in test fixture: {san_uri:?}: {e}")
            });

        let mut cache = self.kiseki_tenant_certs.lock().unwrap();
        if let Some(c) = cache.get(tenant_id) {
            return TenantClientCert {
                san_uri: c.san_uri.clone(),
                cert_pem: c.cert_pem.clone(),
                key_pem: c.key_pem.clone(),
            };
        }
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::NoCa;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, format!("kiseki-tenant-{tenant_id}"));
        params
            .subject_alt_names
            .push(rcgen::SanType::DnsName("localhost".try_into().unwrap()));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
        params
            .subject_alt_names
            .push(rcgen::SanType::URI(san_uri.clone().try_into().unwrap()));
        let key = KeyPair::generate().unwrap();
        let cert = params
            .signed_by(&key, &*self.ca_issuer.lock().unwrap())
            .unwrap();
        let entry = TenantClientCert {
            san_uri: san_uri.clone(),
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        };
        cache.insert(tenant_id.to_string(), TenantClientCert {
            san_uri: entry.san_uri.clone(),
            cert_pem: entry.cert_pem.clone(),
            key_pem: entry.key_pem.clone(),
        });
        entry
    }

    /// Mint a cert with an arbitrary SAN URI string. Used for the
    /// canonicalization near-miss outline (trailing slash, mixed case,
    /// percent-encoded, IDN homograph) — those URIs would fail the
    /// `mint_kiseki_tenant_cert` canonicalizer so they need a back
    /// door. **Test-only.**
    pub fn mint_cert_with_raw_san(&self, common_name: &str, san_uri: &str) -> TenantClientCert {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::NoCa;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params
            .subject_alt_names
            .push(rcgen::SanType::DnsName("localhost".try_into().unwrap()));
        params
            .subject_alt_names
            .push(rcgen::SanType::IpAddress("127.0.0.1".parse().unwrap()));
        params
            .subject_alt_names
            .push(rcgen::SanType::URI(san_uri.try_into().unwrap()));
        let key = KeyPair::generate().unwrap();
        let cert = params
            .signed_by(&key, &*self.ca_issuer.lock().unwrap())
            .unwrap();
        TenantClientCert {
            san_uri: san_uri.to_string(),
            cert_pem: cert.pem(),
            key_pem: key.serialize_pem(),
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
