//! Native binding selector — orchestrates per-binding probe + port
//! collision detection + operator-pin filtering. ADR-042 §3.1.
//!
//! The selector does NOT spawn listeners — that's the runtime's job.
//! The selector outputs a [`SelectorPlan`]: the set of bindings that
//! are `Available` and unmasked by an operator pin, in priority
//! order (highest latency_class first). The runtime walks the plan
//! and spawns each.
//!
//! Per-binding probes implement [`BindingProbe`]. The trait is
//! `async` and timeout-bounded by the selector — probes that hang
//! past the budget self-disqualify with `probe_timeout_exceeded`.

use std::collections::BTreeMap;
use std::time::Duration;

use kiseki_proto::native_contract::{BindingId, LatencyClass, ListenAddr};

/// Default probe budget per binding (ADR-042 §3.1 phase 1).
/// `KISEKI_NATIVE_PROBE_TIMEOUT_MS` overrides at runtime.
pub const DEFAULT_PROBE_TIMEOUT_MS: u64 = 3_000;

/// Operator pin via `KISEKI_NATIVE_TRANSPORT`. `Auto` is the literal
/// "no pin" sentinel — clients and servers ranked by latency_class.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperatorPin {
    /// No pin — selector ranks by latency_class.
    Auto,
    /// Pin to a specific binding. Other bindings still execute
    /// phase 1 + phase 2 (collision detection still relevant for
    /// diagnosis), but phase 3 only spawns the pinned listener.
    Pinned(BindingId),
}

impl OperatorPin {
    /// Parse a pin from the env-var value. Empty / unset / `auto`
    /// (case-insensitive) → `Auto`. Known names → `Pinned`.
    /// Unknown strings → `Err` so operator typos fail loud.
    ///
    /// # Errors
    /// `Err(BindingSelectorError::UnknownPin)` for unrecognized pin
    /// names — operator typo / pre-`auto` syntax.
    pub fn parse(env_value: Option<&str>) -> Result<Self, BindingSelectorError> {
        let trimmed = env_value.map(str::trim).unwrap_or("");
        if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        match trimmed.to_ascii_lowercase().as_str() {
            "grpc" | "h2" => Ok(Self::Pinned(BindingId::Grpc)),
            "tcp" | "tcp_framed" | "tcp-framed" => Ok(Self::Pinned(BindingId::TcpFramed)),
            "ibverbs" | "verbs" | "rdma_verbs" => Ok(Self::Pinned(BindingId::Ibverbs)),
            "libfabric" | "ofi" => Err(BindingSelectorError::UnknownPin {
                got: trimmed.into(),
                hint: "libfabric pin requires a provider — use \
                       KISEKI_NATIVE_LIBFABRIC_PROVIDER for the provider variant"
                    .into(),
            }),
            other => Err(BindingSelectorError::UnknownPin {
                got: other.into(),
                hint: "valid: auto | grpc | tcp | ibverbs (libfabric needs provider env)".into(),
            }),
        }
    }
}

/// Per-binding probe outcome. Mirrors §3.1 phase 1 vocabulary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// Binding ready to be spawned. `latency_class` becomes the
    /// ranking key in [`BindingSelector::plan`]; `addr` is the
    /// listen address (or fabric descriptor for RDMA bindings).
    Available {
        /// Latency class — `Rdma > Low > Standard` ranking.
        latency_class: LatencyClass,
        /// Listen address (host:port for IP bindings;
        /// fabric-opaque descriptor for RDMA).
        addr: ListenAddr,
    },
    /// Binding self-disqualified. Reason carried verbatim through
    /// to logs / banner / diagnostics.
    Unavailable {
        /// Free-form reason. Stable in the sense that operators may
        /// scrape it — bindings should keep the wording terse and
        /// keyed on the failure mode (e.g. `"libibverbs not present"`,
        /// `"probe_timeout_exceeded"`, `"listener_spawn_failed: <e>"`).
        reason: String,
    },
}

/// Probe contract. Each binding's crate ships an impl. The selector
/// holds `Box<dyn BindingProbe>` registrations.
///
/// `binding_id` is a stable identifier the selector uses for the
/// pin-match + port-collision diagnostics. `probe()` is invoked
/// inside the per-binding timeout — implementations must be cancel-
/// safe (a probe future dropped past timeout MUST NOT leave system
/// state behind).
#[async_trait::async_trait]
pub trait BindingProbe: Send + Sync {
    /// The binding's stable identifier. Used by the selector for
    /// pin-match (`KISEKI_NATIVE_TRANSPORT` env-var alias resolution
    /// is done by the selector, not the probe).
    fn binding_id(&self) -> BindingId;

    /// Probe the local environment. Read-only — no port binds, no
    /// fabric resource acquisition. Selector wraps the future in a
    /// timeout per `KISEKI_NATIVE_PROBE_TIMEOUT_MS`.
    async fn probe(&self) -> ProbeOutcome;
}

/// One available binding, ready for phase-3 listener spawn.
///
/// Ordering: by `latency_class` (Rdma > Low > Standard) descending,
/// then by `binding_id` for stable ties. The selector emits a
/// [`SelectorPlan`] in this order; the runtime walks it head-to-tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AvailableBinding {
    /// Stable binding identifier.
    pub binding_id: BindingId,
    /// Latency class for ranking.
    pub latency_class: LatencyClass,
    /// Listen address.
    pub addr: ListenAddr,
}

/// Ranking key for `LatencyClass`. Higher → better.
fn latency_rank(c: LatencyClass) -> u8 {
    match c {
        LatencyClass::Rdma => 3,
        LatencyClass::Low => 2,
        LatencyClass::Standard => 1,
    }
}

impl AvailableBinding {
    /// Sort key: descending priority. `BinaryHeap` / `sort_by_key`
    /// uses ascending order, so we negate via wrapping sub.
    fn sort_key(&self) -> (i8, BindingId) {
        (-(latency_rank(self.latency_class) as i8), self.binding_id)
    }
}

/// Output of a successful selector run. Walks head-to-tail in
/// priority order to spawn listeners.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorPlan {
    /// Available bindings, ranked. Operator pin (if active) means
    /// only the pinned binding's entry is present here; the others
    /// passed phases 1+2 but are filtered out for spawning.
    pub spawn_order: Vec<AvailableBinding>,
    /// Operator pin in effect (`Auto` if none).
    pub pin: OperatorPin,
}

/// Diagnostic report from a selector run — covers ALL probed
/// bindings, including the unavailable ones. Used for the startup
/// banner and the diagnostic dashboard. Distinct from
/// [`SelectorPlan`] which is the "what to spawn" answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectorReport {
    /// Per-binding probe outcome, in registration order.
    pub probes: Vec<(BindingId, ProbeOutcome)>,
    /// Operator pin in effect.
    pub pin: OperatorPin,
}

/// Selector errors. Mapped to ADR-042 §3.1 fatal conditions; exit
/// codes per the spec table.
#[derive(Debug, thiserror::Error)]
pub enum BindingSelectorError {
    /// Two bindings advertised the same `listen_addr`. Operator
    /// misconfig (overlapping `KISEKI_NATIVE_*_ADDR`). ADR-042 §3.1
    /// phase 2 → exit code 2.
    #[error("port collision between {a:?} and {b:?} at {addr:?}")]
    PortCollision {
        /// First colliding binding.
        a: BindingId,
        /// Second colliding binding.
        b: BindingId,
        /// The collided address.
        addr: ListenAddr,
    },
    /// All registered bindings reported `Unavailable`. ADR-042 §3.1
    /// phase 3 → exit code 3.
    #[error("no bindings available (registered {registered}, all unavailable)")]
    NoAvailableBindings {
        /// Number of bindings that were probed.
        registered: usize,
    },
    /// `KISEKI_NATIVE_TRANSPORT` pin pointed at a binding that
    /// reported `Unavailable`. ADR-042 §3.1 → exit code 4.
    #[error("operator pin {pinned:?} unavailable: {reason}")]
    PinnedBindingUnavailable {
        /// The pinned binding that's not available.
        pinned: BindingId,
        /// Reason from the probe.
        reason: String,
    },
    /// `KISEKI_NATIVE_TRANSPORT` pin pointed at a binding that's not
    /// even registered (compile-time decision). ADR-042 §3.1 → exit
    /// code 4.
    #[error("operator pin {pinned:?} not registered (binding not compiled in)")]
    PinnedBindingNotRegistered {
        /// The pinned binding that wasn't registered.
        pinned: BindingId,
    },
    /// `KISEKI_NATIVE_TRANSPORT` env-var value didn't parse.
    #[error("unknown KISEKI_NATIVE_TRANSPORT value {got:?}: {hint}")]
    UnknownPin {
        /// The unrecognized value from the env var.
        got: String,
        /// Hint string for diagnostics.
        hint: String,
    },
}

/// Native binding selector. Owns registered probes; exposes
/// `plan()` to run the orchestration end-to-end and emit a
/// `(plan, report)` pair.
///
/// Probes are kept in a `Vec` rather than a `HashMap` so the
/// selector preserves registration order in the diagnostic report
/// (helpful for operators inspecting startup logs).
pub struct BindingSelector {
    probes: Vec<Box<dyn BindingProbe>>,
    probe_timeout: Duration,
    pin: OperatorPin,
}

impl BindingSelector {
    /// Build an empty selector. Use [`register`](Self::register) to
    /// add per-binding probes; [`with_probe_timeout`](Self::with_probe_timeout)
    /// and [`with_pin`](Self::with_pin) configure runtime behavior.
    #[must_use]
    pub fn new() -> Self {
        Self {
            probes: Vec::new(),
            probe_timeout: Duration::from_millis(DEFAULT_PROBE_TIMEOUT_MS),
            pin: OperatorPin::Auto,
        }
    }

    /// Register a per-binding probe. Order matters only for
    /// diagnostic output; the spawn order is determined by
    /// latency_class ranking.
    pub fn register(&mut self, probe: Box<dyn BindingProbe>) -> &mut Self {
        self.probes.push(probe);
        self
    }

    /// Override the probe timeout. Default
    /// [`DEFAULT_PROBE_TIMEOUT_MS`].
    #[must_use]
    pub fn with_probe_timeout(mut self, timeout: Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Set the operator pin. Default [`OperatorPin::Auto`].
    #[must_use]
    pub fn with_pin(mut self, pin: OperatorPin) -> Self {
        self.pin = pin;
        self
    }

    /// Run phases 1 + 2 + filter for the operator pin. Returns the
    /// `(plan, report)` pair the runtime uses to spawn listeners
    /// and emit the startup banner.
    ///
    /// # Errors
    /// - `PortCollision` — phase 2 fatal.
    /// - `NoAvailableBindings` — phase 3 precondition failed.
    /// - `PinnedBindingUnavailable` / `PinnedBindingNotRegistered`
    ///   — operator pin unsatisfiable.
    pub async fn plan(self) -> Result<(SelectorPlan, SelectorReport), BindingSelectorError> {
        // Phase 1 — probe all, sequential.
        let mut probes_results: Vec<(BindingId, ProbeOutcome)> =
            Vec::with_capacity(self.probes.len());
        for probe in &self.probes {
            let id = probe.binding_id();
            let outcome = match tokio::time::timeout(self.probe_timeout, probe.probe()).await {
                Ok(o) => o,
                Err(_) => ProbeOutcome::Unavailable {
                    reason: format!(
                        "probe_timeout_exceeded ({} ms)",
                        self.probe_timeout.as_millis()
                    ),
                },
            };
            probes_results.push((id, outcome));
        }

        // Phase 2 — port-collision check on the Available subset.
        check_port_collisions(&probes_results)?;

        // Build the available list, sorted by priority (Rdma > Low > Standard).
        let mut available: Vec<AvailableBinding> = probes_results
            .iter()
            .filter_map(|(id, outcome)| match outcome {
                ProbeOutcome::Available {
                    latency_class,
                    addr,
                } => Some(AvailableBinding {
                    binding_id: *id,
                    latency_class: *latency_class,
                    addr: addr.clone(),
                }),
                ProbeOutcome::Unavailable { .. } => None,
            })
            .collect();
        available.sort_by_key(AvailableBinding::sort_key);

        // Apply the operator pin: filter the spawn list to just the
        // pinned binding (or fail if it's unavailable / not
        // registered). Keep the report unchanged so diagnostics
        // still show the full probe set.
        let spawn_order = match self.pin {
            OperatorPin::Auto => available,
            OperatorPin::Pinned(target) => {
                let registered = probes_results.iter().any(|(id, _)| *id == target);
                if !registered {
                    return Err(BindingSelectorError::PinnedBindingNotRegistered {
                        pinned: target,
                    });
                }
                let pinned_outcome = probes_results
                    .iter()
                    .find_map(|(id, o)| (*id == target).then_some(o));
                match pinned_outcome {
                    Some(ProbeOutcome::Available { .. }) => available
                        .into_iter()
                        .filter(|b| b.binding_id == target)
                        .collect(),
                    Some(ProbeOutcome::Unavailable { reason }) => {
                        return Err(BindingSelectorError::PinnedBindingUnavailable {
                            pinned: target,
                            reason: reason.clone(),
                        });
                    }
                    None => {
                        // Already covered by !registered above; defense in depth.
                        return Err(BindingSelectorError::PinnedBindingNotRegistered {
                            pinned: target,
                        });
                    }
                }
            }
        };

        if spawn_order.is_empty() {
            return Err(BindingSelectorError::NoAvailableBindings {
                registered: probes_results.len(),
            });
        }

        Ok((
            SelectorPlan {
                spawn_order,
                pin: self.pin,
            },
            SelectorReport {
                probes: probes_results,
                pin: self.pin,
            },
        ))
    }
}

impl Default for BindingSelector {
    fn default() -> Self {
        Self::new()
    }
}

/// Phase-2 helper: detect address collisions between Available
/// bindings. Pure function, no async; tested in isolation.
fn check_port_collisions(probes: &[(BindingId, ProbeOutcome)]) -> Result<(), BindingSelectorError> {
    // Map address → first binding that claimed it.
    let mut seen: BTreeMap<&ListenAddr, BindingId> = BTreeMap::new();
    for (id, outcome) in probes {
        if let ProbeOutcome::Available { addr, .. } = outcome {
            if let Some(prior) = seen.insert(addr, *id) {
                return Err(BindingSelectorError::PortCollision {
                    a: prior,
                    b: *id,
                    addr: addr.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Render the §3.1 startup banner from a [`SelectorReport`]. Pure
/// formatting, no I/O — runtime emits via `tracing::info`.
#[must_use]
pub fn render_banner(plan: &SelectorPlan, report: &SelectorReport) -> String {
    let mut out = String::new();
    out.push_str("[transport.native] available bindings (in priority order):\n");
    if plan.spawn_order.is_empty() {
        out.push_str("  (none — no bindings will be spawned)\n");
    } else {
        for (idx, b) in plan.spawn_order.iter().enumerate() {
            out.push_str(&format!(
                "  {}. {:<16} (latency_class={:?}, addr={})\n",
                idx + 1,
                binding_display(b.binding_id),
                b.latency_class,
                listen_addr_display(&b.addr),
            ));
        }
    }
    let unavailable_count = report
        .probes
        .iter()
        .filter(|(_, o)| matches!(o, ProbeOutcome::Unavailable { .. }))
        .count();
    if unavailable_count > 0 {
        out.push_str("[transport.native] unavailable bindings:\n");
        for (id, outcome) in &report.probes {
            if let ProbeOutcome::Unavailable { reason } = outcome {
                out.push_str(&format!(
                    "  - {:<16} skipped: {}\n",
                    binding_display(*id),
                    reason
                ));
            }
        }
    }
    match plan.pin {
        OperatorPin::Auto => out.push_str(
            "[transport.native] override available via KISEKI_NATIVE_TRANSPORT={grpc|tcp|ibverbs|libfabric|auto}\n",
        ),
        OperatorPin::Pinned(target) => out.push_str(&format!(
            "[transport.native] PINNED to {} via KISEKI_NATIVE_TRANSPORT (other bindings probed but not spawned)\n",
            binding_display(target)
        )),
    }
    out
}

fn binding_display(id: BindingId) -> &'static str {
    match id {
        BindingId::Grpc => "grpc-h2",
        BindingId::TcpFramed => "tcp-framed",
        BindingId::Ibverbs => "ibverbs",
        BindingId::Libfabric { .. } => "libfabric",
    }
}

fn listen_addr_display(addr: &ListenAddr) -> String {
    match addr {
        ListenAddr::HostPort(s) => s.clone(),
        ListenAddr::FabricDescriptor(bytes) => format!("<fabric:{} bytes>", bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Stub probe — emits a configurable outcome on each call. Used
    /// by every test to avoid pulling real bindings into the
    /// transport-layer test.
    struct StubProbe {
        id: BindingId,
        outcome: ProbeOutcome,
        invocations: AtomicU32,
    }

    impl StubProbe {
        fn new(id: BindingId, outcome: ProbeOutcome) -> Self {
            Self {
                id,
                outcome,
                invocations: AtomicU32::new(0),
            }
        }

        fn boxed(id: BindingId, outcome: ProbeOutcome) -> Box<Self> {
            Box::new(Self::new(id, outcome))
        }
    }

    #[async_trait::async_trait]
    impl BindingProbe for StubProbe {
        fn binding_id(&self) -> BindingId {
            self.id
        }
        async fn probe(&self) -> ProbeOutcome {
            self.invocations.fetch_add(1, Ordering::Relaxed);
            self.outcome.clone()
        }
    }

    fn host_port(s: &str) -> ListenAddr {
        ListenAddr::HostPort(s.into())
    }

    #[test]
    fn parse_pin_auto_handles_empty_unset_and_explicit_auto() {
        for input in [None, Some(""), Some("   "), Some("auto"), Some("AUTO")] {
            let pin = OperatorPin::parse(input).expect(&format!("input {input:?}"));
            assert_eq!(pin, OperatorPin::Auto);
        }
    }

    #[test]
    fn parse_pin_known_bindings() {
        assert_eq!(
            OperatorPin::parse(Some("grpc")).unwrap(),
            OperatorPin::Pinned(BindingId::Grpc)
        );
        assert_eq!(
            OperatorPin::parse(Some("h2")).unwrap(),
            OperatorPin::Pinned(BindingId::Grpc)
        );
        assert_eq!(
            OperatorPin::parse(Some("tcp")).unwrap(),
            OperatorPin::Pinned(BindingId::TcpFramed)
        );
        assert_eq!(
            OperatorPin::parse(Some("tcp_framed")).unwrap(),
            OperatorPin::Pinned(BindingId::TcpFramed)
        );
        assert_eq!(
            OperatorPin::parse(Some("ibverbs")).unwrap(),
            OperatorPin::Pinned(BindingId::Ibverbs)
        );
    }

    #[test]
    fn parse_pin_libfabric_requires_provider_env() {
        let err = OperatorPin::parse(Some("libfabric")).expect_err("must reject");
        match err {
            BindingSelectorError::UnknownPin { hint, .. } => {
                assert!(hint.contains("KISEKI_NATIVE_LIBFABRIC_PROVIDER"));
            }
            other => panic!("expected UnknownPin, got: {other:?}"),
        }
    }

    #[test]
    fn parse_pin_typo_is_rejected() {
        let err = OperatorPin::parse(Some("gRRPC")).expect_err("typo");
        assert!(matches!(err, BindingSelectorError::UnknownPin { .. }));
    }

    #[test]
    fn latency_rank_orders_rdma_above_low_above_standard() {
        assert!(latency_rank(LatencyClass::Rdma) > latency_rank(LatencyClass::Low));
        assert!(latency_rank(LatencyClass::Low) > latency_rank(LatencyClass::Standard));
    }

    #[test]
    fn check_port_collisions_passes_on_distinct_addrs() {
        let probes = vec![
            (
                BindingId::Grpc,
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Standard,
                    addr: host_port("0.0.0.0:9100"),
                },
            ),
            (
                BindingId::TcpFramed,
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Low,
                    addr: host_port("0.0.0.0:9101"),
                },
            ),
        ];
        check_port_collisions(&probes).expect("distinct addrs ok");
    }

    #[test]
    fn check_port_collisions_detects_collision() {
        let probes = vec![
            (
                BindingId::Grpc,
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Standard,
                    addr: host_port("0.0.0.0:9100"),
                },
            ),
            (
                BindingId::TcpFramed,
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Low,
                    addr: host_port("0.0.0.0:9100"), // collision!
                },
            ),
        ];
        let err = check_port_collisions(&probes).expect_err("must collide");
        match err {
            BindingSelectorError::PortCollision { a, b, addr } => {
                assert_eq!(a, BindingId::Grpc);
                assert_eq!(b, BindingId::TcpFramed);
                assert_eq!(addr, host_port("0.0.0.0:9100"));
            }
            other => panic!("expected PortCollision, got: {other:?}"),
        }
    }

    #[test]
    fn check_port_collisions_ignores_unavailable_addrs() {
        // Unavailable bindings have no addr to collide on. Even if
        // two were "available" at the same address we should detect;
        // an unavailable can't trigger a collision.
        let probes = vec![
            (
                BindingId::Grpc,
                ProbeOutcome::Available {
                    latency_class: LatencyClass::Standard,
                    addr: host_port("0.0.0.0:9100"),
                },
            ),
            (
                BindingId::Ibverbs,
                ProbeOutcome::Unavailable {
                    reason: "no rdma hw".into(),
                },
            ),
        ];
        check_port_collisions(&probes).expect("ok with unavail");
    }

    #[tokio::test]
    async fn plan_with_two_available_bindings_orders_by_latency() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::TcpFramed,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Low,
                addr: host_port("0.0.0.0:9101"),
            },
        ));
        let (plan, report) = sel.plan().await.unwrap();
        assert_eq!(plan.spawn_order.len(), 2);
        // TcpFramed (Low) should rank above Grpc (Standard).
        assert_eq!(plan.spawn_order[0].binding_id, BindingId::TcpFramed);
        assert_eq!(plan.spawn_order[1].binding_id, BindingId::Grpc);
        assert_eq!(report.probes.len(), 2);
    }

    #[tokio::test]
    async fn plan_returns_no_available_when_all_unavailable() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Unavailable { reason: "x".into() },
        ));
        sel.register(StubProbe::boxed(
            BindingId::TcpFramed,
            ProbeOutcome::Unavailable { reason: "y".into() },
        ));
        let err = sel.plan().await.expect_err("must fail");
        match err {
            BindingSelectorError::NoAvailableBindings { registered } => {
                assert_eq!(registered, 2);
            }
            other => panic!("expected NoAvailableBindings, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn pin_to_unavailable_binding_fails() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::Ibverbs,
            ProbeOutcome::Unavailable {
                reason: "no rdma hw".into(),
            },
        ));
        let sel = sel.with_pin(OperatorPin::Pinned(BindingId::Ibverbs));
        let err = sel.plan().await.expect_err("must fail");
        match err {
            BindingSelectorError::PinnedBindingUnavailable { pinned, reason } => {
                assert_eq!(pinned, BindingId::Ibverbs);
                assert!(reason.contains("no rdma hw"));
            }
            other => panic!("expected PinnedBindingUnavailable, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn pin_to_unregistered_binding_fails() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        // Pin to TcpFramed which wasn't registered (e.g., compiled out).
        let sel = sel.with_pin(OperatorPin::Pinned(BindingId::TcpFramed));
        let err = sel.plan().await.expect_err("must fail");
        match err {
            BindingSelectorError::PinnedBindingNotRegistered { pinned } => {
                assert_eq!(pinned, BindingId::TcpFramed);
            }
            other => panic!("expected PinnedBindingNotRegistered, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn pin_to_available_binding_filters_spawn_to_just_pinned() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::TcpFramed,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Low,
                addr: host_port("0.0.0.0:9101"),
            },
        ));
        let sel = sel.with_pin(OperatorPin::Pinned(BindingId::Grpc));
        let (plan, report) = sel.plan().await.unwrap();
        // Only the pinned binding is spawned, even though TcpFramed
        // would normally outrank by latency.
        assert_eq!(plan.spawn_order.len(), 1);
        assert_eq!(plan.spawn_order[0].binding_id, BindingId::Grpc);
        // Report still shows BOTH probes — diagnosis stays full.
        assert_eq!(report.probes.len(), 2);
    }

    #[tokio::test]
    async fn port_collision_short_circuits_plan() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::TcpFramed,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Low,
                addr: host_port("0.0.0.0:9100"), // <-- collision
            },
        ));
        let err = sel.plan().await.expect_err("collision");
        assert!(matches!(err, BindingSelectorError::PortCollision { .. }));
    }

    /// Probe that hangs forever — the selector's per-binding
    /// timeout must surface this as `probe_timeout_exceeded`.
    struct HangingProbe;
    #[async_trait::async_trait]
    impl BindingProbe for HangingProbe {
        fn binding_id(&self) -> BindingId {
            BindingId::Grpc
        }
        async fn probe(&self) -> ProbeOutcome {
            std::future::pending::<()>().await;
            unreachable!()
        }
    }

    #[tokio::test]
    async fn hanging_probe_times_out_with_recognizable_reason() {
        let mut sel = BindingSelector::new();
        sel.register(Box::new(HangingProbe));
        // Tiny timeout so the test runs fast.
        let sel = sel.with_probe_timeout(Duration::from_millis(50));
        let err = sel.plan().await.expect_err("all-unavail after timeout");
        // After timeout the only binding is Unavailable → NoAvailableBindings.
        assert!(matches!(
            err,
            BindingSelectorError::NoAvailableBindings { registered: 1 }
        ));
    }

    #[tokio::test]
    async fn banner_lists_spawn_order_and_skipped_bindings() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("10.0.0.42:9100"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::TcpFramed,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Low,
                addr: host_port("10.0.0.42:9101"),
            },
        ));
        sel.register(StubProbe::boxed(
            BindingId::Ibverbs,
            ProbeOutcome::Unavailable {
                reason: "libibverbs not present".into(),
            },
        ));
        let (plan, report) = sel.plan().await.unwrap();
        let banner = render_banner(&plan, &report);
        assert!(banner.contains("tcp-framed"));
        assert!(banner.contains("grpc-h2"));
        assert!(banner.contains("10.0.0.42:9101"));
        assert!(banner.contains("ibverbs"));
        assert!(banner.contains("libibverbs not present"));
        assert!(banner.contains("KISEKI_NATIVE_TRANSPORT"));
        // Spawn-order has TcpFramed first (Low > Standard).
        let tf_pos = banner.find("tcp-framed").unwrap();
        let grpc_pos = banner.find("grpc-h2").unwrap();
        assert!(
            tf_pos < grpc_pos,
            "tcp-framed should be listed before grpc-h2 in the banner"
        );
    }

    #[tokio::test]
    async fn banner_indicates_active_pin() {
        let mut sel = BindingSelector::new();
        sel.register(StubProbe::boxed(
            BindingId::Grpc,
            ProbeOutcome::Available {
                latency_class: LatencyClass::Standard,
                addr: host_port("0.0.0.0:9100"),
            },
        ));
        let sel = sel.with_pin(OperatorPin::Pinned(BindingId::Grpc));
        let (plan, report) = sel.plan().await.unwrap();
        let banner = render_banner(&plan, &report);
        assert!(
            banner.contains("PINNED"),
            "banner should indicate pin: {banner}"
        );
        assert!(banner.contains("grpc-h2"));
    }
}
