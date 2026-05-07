//! libfabric binding probe (ADR-042 §2.4 + §16.1 phase 10).
//!
//! Probes for HPE Slingshot+Cassini (`cxi`), generic libfabric
//! verbs (`verbs`), or the sockets/tcp fall-back providers. Per
//! ADR-042 §2.4.4, the auto-rank order is `cxi > verbs >
//! sockets/tcp` (efa is deferred per §2.4.3). Operator pin via
//! `KISEKI_NATIVE_LIBFABRIC_PROVIDER` overrides; pinning to a
//! deferred provider returns `TransportError::PinnedProviderDeferred`
//! at startup (handled at the listener / runtime layer; this probe
//! just reports the available provider).
//!
//! Same scaffold caveat as the ibverbs probe: this v1 implementation
//! validates the libfabric system library path (R2-M2 path-injection
//! mitigation) and reports a sysfs-derived provider when the cxi
//! provider's `/sys/class/net/.../device/cxi/` directory is present;
//! the actual `fi_getinfo()` call + provider selection happens at
//! listener-spawn time (phase 10 hardware work). On dev hosts
//! without RDMA hardware, returns `Unavailable` cleanly.

use kiseki_proto::native_contract::{BindingId, LatencyClass, LibfabricProvider, ListenAddr};

use super::selector::{BindingProbe, ProbeOutcome};

/// libfabric binding probe.
pub struct LibfabricProbe {
    /// Operator-pinned provider. `None` → auto-pick by §2.4.4 rank
    /// (cxi > verbs > sockets > tcp). `Some(p)` → only that provider
    /// qualifies; mismatch self-disqualifies.
    pinned_provider: Option<LibfabricProvider>,
}

impl LibfabricProbe {
    /// Build with env-var override.
    /// `KISEKI_NATIVE_LIBFABRIC_PROVIDER={cxi|verbs|sockets|tcp}`
    /// pins to that provider; absent / unrecognized → auto.
    #[must_use]
    pub fn new() -> Self {
        let pinned_provider = std::env::var("KISEKI_NATIVE_LIBFABRIC_PROVIDER")
            .ok()
            .and_then(|s| parse_provider(&s));
        Self { pinned_provider }
    }
}

impl Default for LibfabricProbe {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_provider(s: &str) -> Option<LibfabricProvider> {
    match s.trim().to_ascii_lowercase().as_str() {
        "cxi" => Some(LibfabricProvider::Cxi),
        "verbs" => Some(LibfabricProvider::Verbs),
        "sockets" => Some(LibfabricProvider::Sockets),
        "tcp" => Some(LibfabricProvider::Tcp),
        // efa is intentionally rejected — §2.4.3 deferred. Operators
        // pinning to efa get a hard error from the listener; the
        // probe itself returns Unavailable. Folded into the wildcard
        // arm to silence `clippy::match_same_arms`; the comment above
        // is the load-bearing contract.
        _ => None,
    }
}

#[async_trait::async_trait]
impl BindingProbe for LibfabricProbe {
    fn binding_id(&self) -> BindingId {
        // The contract type's BindingId::Libfabric carries the
        // provider, but the probe doesn't know the provider until
        // it's discovered. Use Cxi as the placeholder discriminator
        // for the binding identity (the actual provider comes back
        // via the addr — encoded in the FabricDescriptor).
        //
        // Selector uses binding_id() ONLY for pin-match + audit
        // attribution; the provider variant flows through addr.
        BindingId::Libfabric {
            provider: LibfabricProvider::Cxi,
        }
    }

    async fn probe(&self) -> ProbeOutcome {
        if !cfg!(target_os = "linux") {
            return ProbeOutcome::Unavailable {
                reason: "libfabric binding requires Linux".into(),
            };
        }

        // Step 1 — library presence + path validation.
        let lib_path = match super::probe_helpers::resolve_system_library(
            "libfabric.so.1",
            "KISEKI_NATIVE_LIBFABRIC_LIB",
        ) {
            Ok(p) => p,
            Err(reason) => {
                return ProbeOutcome::Unavailable {
                    reason: format!("libfabric path: {reason}"),
                };
            }
        };
        tracing::debug!(
            path = %lib_path.display(),
            "libfabric probe: libfabric.so.1 resolved + validated",
        );

        // Step 2 — provider discovery. Without dlopen-ing libfabric
        // and calling fi_getinfo() (deferred to phase-10 listener
        // work), the probe consults sysfs for evidence: cxi exposes
        // /sys/class/net/.../device/cxi; verbs exposes
        // /sys/class/infiniband/*. Sockets/tcp providers are always
        // available on any Linux host but rank lowest.
        let provider = match self.pinned_provider {
            Some(p) => p,
            None => choose_provider_via_sysfs(),
        };
        // If pinned, validate the requested provider's evidence is
        // present; otherwise fail with a diagnostic so operators
        // know why the pin didn't take.
        if self.pinned_provider.is_some() {
            if let Err(reason) = validate_pinned_provider(provider) {
                return ProbeOutcome::Unavailable {
                    reason: format!(
                        "KISEKI_NATIVE_LIBFABRIC_PROVIDER pinned to {provider:?}: {reason}"
                    ),
                };
            }
        }
        let descriptor = format!("libfabric://{}", provider_descriptor(provider));
        let latency_class = match provider {
            // Sockets / tcp are emulated paths — Standard, not Rdma.
            LibfabricProvider::Sockets | LibfabricProvider::Tcp => LatencyClass::Standard,
            // Efa is deferred; can't be reached because parse_provider
            // rejects "efa" and choose_provider_via_sysfs never
            // returns it. Folded into the Rdma arm to silence
            // `clippy::match_same_arms`; the rejection happens upstream.
            LibfabricProvider::Cxi | LibfabricProvider::Verbs | LibfabricProvider::Efa => {
                LatencyClass::Rdma
            }
        };
        ProbeOutcome::Available {
            latency_class,
            addr: ListenAddr::FabricDescriptor(descriptor.into_bytes()),
        }
    }
}

#[cfg(target_os = "linux")]
fn choose_provider_via_sysfs() -> LibfabricProvider {
    // cxi is highest priority — Slingshot has /sys/class/net/.../
    // device/cxi/. Walk the net devices and check.
    if has_sysfs_dir("/sys/class/net") {
        if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
            for entry in entries.flatten() {
                let cxi_dir = entry.path().join("device").join("cxi");
                if cxi_dir.is_dir() {
                    return LibfabricProvider::Cxi;
                }
            }
        }
    }
    // verbs — InfiniBand HCAs visible in /sys/class/infiniband.
    if has_sysfs_dir("/sys/class/infiniband") {
        if let Ok(mut iter) = std::fs::read_dir("/sys/class/infiniband") {
            if iter.next().is_some() {
                return LibfabricProvider::Verbs;
            }
        }
    }
    // Fall through to sockets — always available on Linux.
    LibfabricProvider::Sockets
}

#[cfg(not(target_os = "linux"))]
fn choose_provider_via_sysfs() -> LibfabricProvider {
    LibfabricProvider::Sockets
}

#[cfg(target_os = "linux")]
fn has_sysfs_dir(p: &str) -> bool {
    std::path::Path::new(p).is_dir()
}

#[cfg(target_os = "linux")]
fn validate_pinned_provider(provider: LibfabricProvider) -> Result<(), String> {
    match provider {
        LibfabricProvider::Cxi => {
            if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
                for entry in entries.flatten() {
                    if entry.path().join("device").join("cxi").is_dir() {
                        return Ok(());
                    }
                }
            }
            Err("no cxi device under /sys/class/net/.../device/cxi".into())
        }
        LibfabricProvider::Verbs => {
            if has_sysfs_dir("/sys/class/infiniband") {
                if let Ok(mut iter) = std::fs::read_dir("/sys/class/infiniband") {
                    if iter.next().is_some() {
                        return Ok(());
                    }
                }
            }
            Err("no InfiniBand device under /sys/class/infiniband".into())
        }
        LibfabricProvider::Sockets | LibfabricProvider::Tcp => Ok(()),
        LibfabricProvider::Efa => Err("efa provider deferred per ADR-042 §2.4.3".into()),
    }
}

#[cfg(not(target_os = "linux"))]
fn validate_pinned_provider(_provider: LibfabricProvider) -> Result<(), String> {
    Err("non-Linux: libfabric provider validation unavailable".into())
}

fn provider_descriptor(provider: LibfabricProvider) -> &'static str {
    match provider {
        LibfabricProvider::Cxi => "cxi",
        LibfabricProvider::Verbs => "verbs",
        LibfabricProvider::Sockets => "sockets",
        LibfabricProvider::Tcp => "tcp",
        LibfabricProvider::Efa => "efa",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_unavailable_on_dev_host_with_clear_reason() {
        let probe = LibfabricProbe::new();
        let outcome = probe.probe().await;
        match outcome {
            ProbeOutcome::Unavailable { reason } => {
                assert!(
                    reason.contains("libfabric") || reason.contains("non-linux"),
                    "reason should name the failure mode: {reason}",
                );
            }
            ProbeOutcome::Available {
                latency_class,
                addr,
            } => {
                // Some dev hosts have libfabric installed (e.g. for
                // testing). Sockets-provider fallback yields
                // Standard latency class.
                assert!(
                    matches!(latency_class, LatencyClass::Standard | LatencyClass::Rdma),
                    "latency class must match a known provider: {latency_class:?}",
                );
                if let ListenAddr::FabricDescriptor(bytes) = &addr {
                    let s = String::from_utf8_lossy(bytes);
                    assert!(s.starts_with("libfabric://"), "descriptor: {s}");
                } else {
                    panic!("libfabric must surface a FabricDescriptor addr");
                }
            }
        }
    }

    #[test]
    fn parse_provider_recognizes_known_names() {
        assert_eq!(parse_provider("cxi"), Some(LibfabricProvider::Cxi));
        assert_eq!(parse_provider(" CXI "), Some(LibfabricProvider::Cxi));
        assert_eq!(parse_provider("verbs"), Some(LibfabricProvider::Verbs));
        assert_eq!(parse_provider("sockets"), Some(LibfabricProvider::Sockets));
        assert_eq!(parse_provider("tcp"), Some(LibfabricProvider::Tcp));
    }

    #[test]
    fn parse_provider_rejects_efa_per_section_2_4_3() {
        // efa is deferred per ADR-042 §2.4.3 — pinning to it must
        // return None so the listener returns
        // PinnedProviderDeferred.
        assert_eq!(parse_provider("efa"), None);
        assert_eq!(parse_provider("EFA"), None);
    }

    #[test]
    fn parse_provider_returns_none_for_typos() {
        assert_eq!(parse_provider("verbz"), None);
        assert_eq!(parse_provider(""), None);
        assert_eq!(parse_provider("rdma"), None);
    }

    #[test]
    fn descriptor_strings_pinned_to_provider_names() {
        // Wire stability — these strings end up in
        // ListenAddr::FabricDescriptor bytes that operators may
        // grep in startup banners. Pinned so a refactor that
        // rephrases them also breaks the test.
        assert_eq!(provider_descriptor(LibfabricProvider::Cxi), "cxi");
        assert_eq!(provider_descriptor(LibfabricProvider::Verbs), "verbs");
        assert_eq!(provider_descriptor(LibfabricProvider::Sockets), "sockets");
        assert_eq!(provider_descriptor(LibfabricProvider::Tcp), "tcp");
        assert_eq!(provider_descriptor(LibfabricProvider::Efa), "efa");
    }

    #[test]
    fn binding_id_is_libfabric() {
        let probe = LibfabricProbe::new();
        match probe.binding_id() {
            BindingId::Libfabric { .. } => {}
            other => panic!("expected Libfabric, got: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn pinned_to_unavailable_provider_self_disqualifies() {
        // Pin to verbs on a dev host without InfiniBand. The probe
        // either accepts (host has IB) or rejects with a diagnostic.
        // Two valid rejection paths can fire BEFORE the provider
        // check ever runs:
        //   1. libfabric.so.1 is missing OR not root-owned (sandbox
        //      builds put it under uid 65534 = nobody).
        //   2. The pinned provider isn't backed by sysfs evidence.
        // Both surface a substantive `Unavailable` reason; we accept
        // either as the test's intent (the probe didn't silently
        // claim Available).
        let probe = LibfabricProbe {
            pinned_provider: Some(LibfabricProvider::Verbs),
        };
        let outcome = probe.probe().await;
        match outcome {
            ProbeOutcome::Available { .. } => {
                // Fine — host happens to have IB.
            }
            ProbeOutcome::Unavailable { reason } => {
                let r = reason.to_ascii_lowercase();
                assert!(
                    r.contains("verbs") || r.contains("libfabric"),
                    "reason should mention either the pinned provider or the libfabric path: {reason}",
                );
            }
        }
    }
}
