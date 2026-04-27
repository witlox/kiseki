//! NFS transport security posture (ADR-038 §D4 / I-PN7).
//!
//! At server boot the runtime calls [`evaluate`] to decide whether the
//! NFS path runs over TLS (default) or in the audited plaintext
//! fallback (operator opt-in). The gate is structurally enforced —
//! both `[security].allow_plaintext_nfs=true` (config) AND
//! `KISEKI_INSECURE_NFS=true` (env) must be set for plaintext, and
//! plaintext is single-tenant only.
//!
//! The function is pure (no env/clock side effects) so it is unit-
//! testable without spinning up a server.

use kiseki_audit::event::AuditEventType;

/// Outcome of evaluating the NFS security posture.
#[derive(Debug, PartialEq, Eq)]
pub struct NfsSecurity {
    /// Active transport mode for the NFS listeners.
    pub mode: NfsTransport,
    /// Effective layout TTL in seconds. Halves to 60s in plaintext
    /// fallback per ADR-038 §D4.2.
    pub effective_layout_ttl_seconds: u64,
    /// Audit event to emit at every boot when the operator has opted
    /// into the security downgrade. `None` for the TLS default.
    pub audit_event: Option<AuditEventType>,
    /// True when the operator opted into plaintext — runtime should
    /// log the WARN banner described in ADR-038 §D4.2.
    pub emit_warn_banner: bool,
}

/// Active transport for the NFS path.
#[derive(Debug, PartialEq, Eq)]
pub enum NfsTransport {
    /// Default — both MDS (`nfs_addr`) and DS (`ds_addr`) terminate
    /// NFS-over-TLS using the existing Cluster CA.
    Tls,
    /// Audited plaintext fallback for older kernels (ADR-038 §D4.2).
    Plaintext,
}

/// Failure modes from [`evaluate`]. Surfaced as a startup error from
/// the runtime — server refuses to start.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NfsSecurityError {
    /// Only one of the two required flags was set. Plaintext NFS is
    /// blocked unless **both** the config flag AND the env var are set.
    #[error("plaintext NFS requires both flags: \
[security].allow_plaintext_nfs=true (config) AND KISEKI_INSECURE_NFS=true (env). \
Got config={config_flag} env={env_flag}")]
    PartialFlags {
        /// State of `[security].allow_plaintext_nfs` in the config.
        config_flag: bool,
        /// State of `KISEKI_INSECURE_NFS` env var.
        env_flag: bool,
    },
    /// Plaintext fallback was opted into but more than one tenant is
    /// served on the listener. ADR-038 §D4.2 forbids this.
    #[error("plaintext NFS is single-tenant only — refusing to start with {tenant_count} tenants on this listener")]
    PlaintextMultiTenant {
        /// Number of tenants mapped to the NFS listener namespace.
        tenant_count: usize,
    },
    /// TLS is the default but no TLS bundle is available. The runtime
    /// must surface a TLS bundle (`KISEKI_CA_PATH` etc.) — running
    /// plaintext requires the explicit fallback.
    #[error("TLS required for the NFS path but no TLS bundle is configured (set KISEKI_CA_PATH/KISEKI_CERT_PATH/KISEKI_KEY_PATH or opt into the plaintext fallback per ADR-038 §D4.2)")]
    TlsBundleMissing,
}

/// Evaluate the NFS security posture from config + env + namespace
/// state. Pure — call site provides all inputs.
///
/// Inputs:
/// - `allow_plaintext_nfs`: from `cfg.allow_plaintext_nfs` (config).
/// - `insecure_env_set`: whether `KISEKI_INSECURE_NFS` is `true`/`1`.
/// - `tls_bundle_present`: whether `cfg.tls.is_some()`.
/// - `default_layout_ttl_seconds`: from `cfg.pnfs.layout_ttl_seconds`.
/// - `tenant_count_on_listener`: number of tenants mapped to the
///   listener's bootstrap namespace (1 for single-tenant deployments).
pub fn evaluate(
    allow_plaintext_nfs: bool,
    insecure_env_set: bool,
    tls_bundle_present: bool,
    default_layout_ttl_seconds: u64,
    tenant_count_on_listener: usize,
) -> Result<NfsSecurity, NfsSecurityError> {
    match (allow_plaintext_nfs, insecure_env_set) {
        // Both flags set: plaintext fallback active.
        (true, true) => {
            if tenant_count_on_listener > 1 {
                return Err(NfsSecurityError::PlaintextMultiTenant {
                    tenant_count: tenant_count_on_listener,
                });
            }
            Ok(NfsSecurity {
                mode: NfsTransport::Plaintext,
                effective_layout_ttl_seconds: 60, // ADR-038 §D4.2 — halved.
                audit_event: Some(AuditEventType::SecurityDowngradeEnabled),
                emit_warn_banner: true,
            })
        }
        // Only one flag set: refuse to start.
        (true, false) | (false, true) => Err(NfsSecurityError::PartialFlags {
            config_flag: allow_plaintext_nfs,
            env_flag: insecure_env_set,
        }),
        // Neither flag: default TLS path.
        (false, false) => {
            if !tls_bundle_present {
                return Err(NfsSecurityError::TlsBundleMissing);
            }
            Ok(NfsSecurity {
                mode: NfsTransport::Tls,
                effective_layout_ttl_seconds: default_layout_ttl_seconds,
                audit_event: None,
                emit_warn_banner: false,
            })
        }
    }
}

/// The exact WARN banner described in ADR-038 §D4.2. Returned as a
/// constant so tests can assert it byte-for-byte.
pub const PLAINTEXT_WARN_BANNER: &str = "NFS path is PLAINTEXT — \
fh4s and data are observable on the network. Mitigations: VPC isolation, \
firewall ingress restrictions. Compliance: this configuration violates \
I-PN7-default and is acceptable only with documented compensating controls.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_default_with_bundle_succeeds() {
        let s = evaluate(false, false, true, 300, 1).expect("ok");
        assert_eq!(s.mode, NfsTransport::Tls);
        assert_eq!(s.effective_layout_ttl_seconds, 300);
        assert!(s.audit_event.is_none());
        assert!(!s.emit_warn_banner);
    }

    #[test]
    fn tls_default_without_bundle_refused() {
        let err = evaluate(false, false, false, 300, 1).unwrap_err();
        assert_eq!(err, NfsSecurityError::TlsBundleMissing);
    }

    #[test]
    fn only_env_set_is_refused() {
        let err = evaluate(false, true, true, 300, 1).unwrap_err();
        assert_eq!(
            err,
            NfsSecurityError::PartialFlags {
                config_flag: false,
                env_flag: true,
            }
        );
    }

    #[test]
    fn only_config_set_is_refused() {
        let err = evaluate(true, false, true, 300, 1).unwrap_err();
        assert_eq!(
            err,
            NfsSecurityError::PartialFlags {
                config_flag: true,
                env_flag: false,
            }
        );
    }

    #[test]
    fn both_flags_single_tenant_yields_plaintext() {
        let s = evaluate(true, true, false, 300, 1).expect("ok");
        assert_eq!(s.mode, NfsTransport::Plaintext);
        assert_eq!(s.effective_layout_ttl_seconds, 60);
        assert_eq!(
            s.audit_event,
            Some(AuditEventType::SecurityDowngradeEnabled)
        );
        assert!(s.emit_warn_banner);
    }

    #[test]
    fn both_flags_multi_tenant_is_refused() {
        let err = evaluate(true, true, false, 300, 2).unwrap_err();
        assert_eq!(
            err,
            NfsSecurityError::PlaintextMultiTenant { tenant_count: 2 }
        );
    }

    #[test]
    fn plaintext_halves_ttl_regardless_of_input() {
        // Even if config asked for a 600s layout TTL, plaintext clamps
        // to 60s per ADR-038 §D4.2.
        let s = evaluate(true, true, false, 600, 1).expect("ok");
        assert_eq!(s.effective_layout_ttl_seconds, 60);
    }

    #[test]
    fn warn_banner_text_is_pinned() {
        // Pin the banner string — any drift requires updating ADR-038
        // and this test together.
        assert!(PLAINTEXT_WARN_BANNER.contains("NFS path is PLAINTEXT"));
        assert!(PLAINTEXT_WARN_BANNER.contains("I-PN7-default"));
        assert!(PLAINTEXT_WARN_BANNER.contains("VPC isolation"));
    }
}
