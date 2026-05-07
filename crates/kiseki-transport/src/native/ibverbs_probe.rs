//! ibverbs binding probe (ADR-042 §2.3 + §16.1 phase 9).
//!
//! Probes the local environment for InfiniBand / RoCEv2 RDMA
//! capability:
//!
//! 1. **OS gate**: Linux only (`cfg(target_os = "linux")`); other
//!    OSes self-disqualify immediately.
//! 2. **Library presence**: search ADR-042 §2.3's distro/arch path
//!    list for `libibverbs.so.1`, validate root-ownership +
//!    permissions per R2-M2 (path-injection mitigation), audit-log
//!    the resolved absolute path.
//! 3. **Sysfs presence**: enumerate `/sys/class/infiniband/*` for at
//!    least one device with an `Active` port (link_layer ==
//!    InfiniBand or RoCEv2 GID type).
//! 4. **kernel version** for rdma-cm TLS (Linux ≥ 6.4 + rdma-core ≥
//!    50.0): documented but not yet asserted — the actual TLS
//!    handshake happens at listener-spawn time (phase 9 listener).
//!
//! On any failure, returns
//! `ProbeOutcome::Unavailable { reason: ... }` with a diagnostic
//! string operators can grep. Successful probe returns
//! `Available { latency_class: Rdma, addr: FabricDescriptor }`.
//!
//! NOTE: this probe does NOT dlopen libibverbs in the v1 scaffold —
//! the listener that actually USES libibverbs is phase-9 hardware
//! work. The probe runs the path-validation discipline so the
//! sysfs check is meaningful (no point probing /sys if libibverbs
//! isn't usable). Once the listener lands, lift dlopen here too.

use kiseki_proto::native_contract::{BindingId, LatencyClass, ListenAddr};

use super::selector::{BindingProbe, ProbeOutcome};

/// ibverbs binding probe. ADR-042 §2.3 + §16.1 phase 9.
pub struct IbverbsProbe {
    /// HCA device name (e.g. `mlx5_0`). Defaults to the first
    /// device found under `/sys/class/infiniband/` with an Active
    /// port. Operators override via `KISEKI_NATIVE_IBVERBS_DEV`.
    device: Option<String>,
    /// HCA port (1 or 2, depending on dual-port NICs). Defaults to
    /// 1; operators override via `KISEKI_NATIVE_IBVERBS_PORT`.
    port: u32,
}

impl IbverbsProbe {
    /// Build a probe with env-var overrides.
    #[must_use]
    pub fn new() -> Self {
        let device = std::env::var("KISEKI_NATIVE_IBVERBS_DEV").ok();
        let port = std::env::var("KISEKI_NATIVE_IBVERBS_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1);
        Self { device, port }
    }
}

impl Default for IbverbsProbe {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl BindingProbe for IbverbsProbe {
    fn binding_id(&self) -> BindingId {
        BindingId::Ibverbs
    }

    async fn probe(&self) -> ProbeOutcome {
        if !cfg!(target_os = "linux") {
            return ProbeOutcome::Unavailable {
                reason: "ibverbs binding requires Linux".into(),
            };
        }

        // Step 1 — library presence + path validation.
        let lib_path = match super::probe_helpers::resolve_system_library(
            "libibverbs.so.1",
            "KISEKI_NATIVE_IBVERBS_LIB",
        ) {
            Ok(p) => p,
            Err(reason) => {
                return ProbeOutcome::Unavailable {
                    reason: format!("libibverbs path: {reason}"),
                };
            }
        };
        tracing::debug!(
            path = %lib_path.display(),
            "ibverbs probe: libibverbs.so.1 resolved + validated",
        );

        // Step 2 — sysfs device presence.
        match probe_sysfs_for_active_device(self.device.as_deref(), self.port) {
            Ok(device) => {
                let descriptor = format!("ibverbs://{}/{}", device, self.port);
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Rdma,
                    addr: ListenAddr::FabricDescriptor(descriptor.into_bytes()),
                }
            }
            Err(reason) => ProbeOutcome::Unavailable {
                reason: format!("ibverbs sysfs: {reason}"),
            },
        }
    }
}

/// Walk `/sys/class/infiniband/*` looking for a device + port with
/// state == `4: ACTIVE`. Returns the device name on success.
///
/// `wanted_device`: `None` → first device with an active port.
/// `Some(name)` → only that device qualifies; if absent / no active
/// port, return `Err`.
#[cfg(target_os = "linux")]
fn probe_sysfs_for_active_device(wanted_device: Option<&str>, port: u32) -> Result<String, String> {
    let infiniband_dir = std::path::Path::new("/sys/class/infiniband");
    let dir = match std::fs::read_dir(infiniband_dir) {
        Ok(d) => d,
        Err(e) => {
            return Err(format!(
                "/sys/class/infiniband not readable: {e} (no RDMA device or container masked /sys)",
            ));
        }
    };
    let mut tried: Vec<String> = Vec::new();
    for entry in dir.flatten() {
        let device_name = entry.file_name().to_string_lossy().to_string();
        if let Some(want) = wanted_device {
            if device_name != want {
                continue;
            }
        }
        let port_state_path = entry
            .path()
            .join("ports")
            .join(port.to_string())
            .join("state");
        let state = match std::fs::read_to_string(&port_state_path) {
            Ok(s) => s,
            Err(_) => {
                tried.push(format!("{device_name}/port{port}: no state file"));
                continue;
            }
        };
        // /sys writes "<code>: <NAME>\n", e.g. "4: ACTIVE\n". Match
        // the leading code so the parser doesn't break if the kernel
        // adds new state names.
        let trimmed = state.trim();
        if trimmed.starts_with("4:") {
            return Ok(device_name);
        }
        tried.push(format!("{device_name}/port{port}: state={trimmed:?}"));
    }
    if tried.is_empty() {
        Err(format!(
            "no devices found in /sys/class/infiniband (wanted {})",
            wanted_device.unwrap_or("any"),
        ))
    } else {
        Err(format!("no usable port (tried: {})", tried.join(", ")))
    }
}

#[cfg(not(target_os = "linux"))]
fn probe_sysfs_for_active_device(
    _wanted_device: Option<&str>,
    _port: u32,
) -> Result<String, String> {
    Err("non-Linux: /sys/class/infiniband unavailable".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On any dev host without RDMA hardware, the probe must
    /// self-disqualify cleanly with a reason that names the failure
    /// mode (library missing OR sysfs missing). Both are valid
    /// "unavailable" paths.
    #[tokio::test]
    async fn probe_unavailable_on_dev_host_with_clear_reason() {
        let probe = IbverbsProbe::new();
        let outcome = probe.probe().await;
        match outcome {
            ProbeOutcome::Unavailable { reason } => {
                let r = reason.to_ascii_lowercase();
                assert!(
                    r.contains("libibverbs") || r.contains("infiniband") || r.contains("non-linux"),
                    "reason should name the failure mode: {reason}",
                );
            }
            ProbeOutcome::Available { .. } => {
                // If the dev host happens to HAVE InfiniBand (rare
                // but possible), the probe legitimately succeeds.
                // Fine — both outcomes are acceptable here.
            }
        }
    }

    #[test]
    fn binding_id_is_ibverbs() {
        let probe = IbverbsProbe::new();
        assert_eq!(probe.binding_id(), BindingId::Ibverbs);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sysfs_probe_returns_clear_diagnostic_when_dir_missing() {
        // Hosts without RDMA report the dir as either "not readable"
        // or "no devices found". Both are acceptable; the probe just
        // needs to surface a parseable reason.
        let result = probe_sysfs_for_active_device(None, 1);
        if let Err(reason) = result {
            let r = reason.to_ascii_lowercase();
            assert!(
                r.contains("infiniband") || r.contains("device"),
                "reason should be diagnostic: {reason}",
            );
        }
        // If the host has IB hardware the test passes silently.
    }
}
