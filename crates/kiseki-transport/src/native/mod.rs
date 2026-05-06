//! Native gateway data-service binding selector + per-binding probe
//! contract (ADR-042 §3.1, §3.2, §16.1 phase 4).
//!
//! Each binding (gRPC, TCP-framed-postcard, ibverbs, libfabric/cxi)
//! ships a [`BindingProbe`] impl. The [`BindingSelector`] orchestrates
//! the three-phase startup:
//!
//! 1. **Probe (read-only)** — call `probe()` per registered binding
//!    with a per-binding timeout (default 3 s, env
//!    `KISEKI_NATIVE_PROBE_TIMEOUT_MS`). Sequential, never parallel —
//!    `dlopen` contention on heavily loaded hosts is real.
//! 2. **Port-conflict check** — verify `Available` listen addresses
//!    are pairwise distinct. Collisions are fatal (exit code 2).
//! 3. **Listener spawn** — orchestrated by the caller. The selector
//!    surfaces the available set and the pin (if any); the runtime
//!    decides per-binding what spawning means.
//!
//! Operator pin via `KISEKI_NATIVE_TRANSPORT={grpc|tcp|ibverbs|
//! libfabric|auto}`. `auto` is the literal "no pin" string.
//! Pinning to an unavailable binding is fatal (exit code 4).
//!
//! Latency-class ranking: `Rdma > Low > Standard`. Coarse on purpose
//! (§3.6 — adaptive ranking is a future-ADR item).

pub mod ibverbs_probe;
pub mod libfabric_probe;
pub mod probe_helpers;
pub mod selector;

pub use ibverbs_probe::IbverbsProbe;
pub use libfabric_probe::LibfabricProbe;
pub use selector::{
    AvailableBinding, BindingProbe, BindingSelector, BindingSelectorError, OperatorPin,
    ProbeOutcome, SelectorPlan, SelectorReport,
};
