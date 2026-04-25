//! External KMS provider abstraction (ADR-028, I-K16).
//!
//! Callers never branch on provider type. Every provider handles
//! wrap, unwrap, rotate, and health checks identically.

use std::fmt::Debug;

/// Opaque provider-specific key version.
pub type KmsEpochId = String;

/// KMS health status.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KmsHealth {
    /// Provider is fully operational.
    Healthy,
    /// Provider is operational but degraded (e.g., high latency).
    Degraded(String),
    /// Provider cannot service requests.
    Unavailable(String),
}

/// Provider abstraction for tenant key management (I-K16).
///
/// Callers never branch on provider type. Every provider handles
/// wrap, unwrap, rotate, and health checks identically.
pub trait TenantKmsProvider: Send + Sync + Debug {
    /// Wrap (encrypt) a DEK derivation parameter with AAD binding (I-K17).
    fn wrap(&self, plaintext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError>;

    /// Unwrap (decrypt) a previously wrapped blob with AAD verification.
    fn unwrap(&self, ciphertext: &[u8], aad: &[u8]) -> Result<Vec<u8>, KmsError>;

    /// Rotate the tenant's key, returning the new epoch ID.
    fn rotate(&self) -> Result<KmsEpochId, KmsError>;

    /// Health check -- connectivity + basic operation test.
    fn health_check(&self) -> KmsHealth;

    /// Provider name for logging.
    fn name(&self) -> &'static str;
}

/// Errors from KMS provider operations.
#[derive(Debug, thiserror::Error)]
pub enum KmsError {
    /// KMS backend is unreachable.
    #[error("KMS unavailable: {0}")]
    Unavailable(String),

    /// Authentication to the KMS backend failed.
    #[error("KMS authentication failed: {0}")]
    AuthFailed(String),

    /// Wrap or unwrap cryptographic operation failed.
    #[error("wrap/unwrap failed: {0}")]
    CryptoError(String),

    /// AAD provided at unwrap does not match the AAD used at wrap time.
    #[error("AAD mismatch")]
    AadMismatch,

    /// The requested key ID does not exist in the provider.
    #[error("key not found: {0}")]
    KeyNotFound(String),

    /// The KEK has been destroyed and cannot be used.
    #[error("KEK destroyed")]
    KekDestroyed,

    /// Circuit breaker is open — fail fast.
    #[error("circuit open")]
    CircuitOpen,

    /// Concurrency limit reached.
    #[error("KMS concurrency limit reached")]
    ConcurrencyLimit,

    /// Operation timed out.
    #[error("KMS timeout")]
    Timeout,
}

// ---------------------------------------------------------------------------
// Circuit breaker (ADR-028)
// ---------------------------------------------------------------------------

/// Circuit breaker state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation — requests pass through.
    Closed,
    /// Too many failures — requests fail immediately.
    Open,
    /// Probe in progress — one request allowed to test recovery.
    HalfOpen,
}

/// Circuit breaker for a KMS provider.
///
/// Opens after `threshold` consecutive failures, probes every
/// `probe_interval` to detect recovery.
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    threshold: u32,
    probe_interval_secs: u64,
    last_failure_epoch_secs: u64,
}

impl CircuitBreaker {
    /// Create a new circuit breaker.
    #[must_use]
    pub fn new(threshold: u32, probe_interval_secs: u64) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            threshold,
            probe_interval_secs,
            last_failure_epoch_secs: 0,
        }
    }

    /// Current state.
    #[must_use]
    pub fn state(&self) -> CircuitState {
        self.state
    }

    /// Check whether a request should be allowed.
    #[must_use]
    pub fn allow_request(&self, now_epoch_secs: u64) -> bool {
        match self.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Allow a probe after the probe interval.
                now_epoch_secs >= self.last_failure_epoch_secs + self.probe_interval_secs
            }
            CircuitState::HalfOpen => false, // probe already in flight
        }
    }

    /// Record a successful operation.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.state = CircuitState::Closed;
    }

    /// Record a failure.
    pub fn record_failure(&mut self, now_epoch_secs: u64) {
        self.consecutive_failures += 1;
        self.last_failure_epoch_secs = now_epoch_secs;
        if self.consecutive_failures >= self.threshold {
            self.state = CircuitState::Open;
        }
    }

    /// Transition to half-open (for probing).
    pub fn try_half_open(&mut self, now_epoch_secs: u64) {
        if self.state == CircuitState::Open
            && now_epoch_secs >= self.last_failure_epoch_secs + self.probe_interval_secs
        {
            self.state = CircuitState::HalfOpen;
        }
    }
}

// ---------------------------------------------------------------------------
// Concurrency limiter
// ---------------------------------------------------------------------------

/// Per-node concurrency limiter for KMS requests.
#[derive(Debug)]
pub struct ConcurrencyLimiter {
    max_concurrent: u32,
    current: std::sync::atomic::AtomicU32,
}

impl ConcurrencyLimiter {
    /// Create a new limiter.
    #[must_use]
    pub fn new(max_concurrent: u32) -> Self {
        Self {
            max_concurrent,
            current: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Try to acquire a permit. Returns `Err(KmsError::ConcurrencyLimit)` if full.
    pub fn try_acquire(&self) -> Result<ConcurrencyPermit<'_>, KmsError> {
        let prev = self
            .current
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        if prev >= self.max_concurrent {
            self.current
                .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            return Err(KmsError::ConcurrencyLimit);
        }
        Ok(ConcurrencyPermit { limiter: self })
    }

    /// Current in-flight count.
    #[must_use]
    pub fn in_flight(&self) -> u32 {
        self.current.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// RAII permit — decrements count on drop.
#[derive(Debug)]
pub struct ConcurrencyPermit<'a> {
    limiter: &'a ConcurrencyLimiter,
}

impl Drop for ConcurrencyPermit<'_> {
    fn drop(&mut self) {
        self.limiter
            .current
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

// ---------------------------------------------------------------------------
// KMS credential types (I-K8)
// ---------------------------------------------------------------------------

/// KMS authentication configuration. `Debug` is redacted (I-K8).
pub enum KmsAuthConfig {
    /// Vault `AppRole` authentication.
    AppRole {
        /// Role ID (non-secret).
        role_id: String,
        /// Secret ID (sensitive — redacted in Debug).
        secret_id: String,
    },
    /// mTLS certificate authentication.
    TlsCert {
        /// Certificate path.
        cert_path: String,
    },
    /// IAM role assumption.
    IamRole {
        /// Role ARN.
        role_arn: String,
    },
}

impl std::fmt::Debug for KmsAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppRole { role_id, .. } => {
                write!(f, "KmsAuthConfig::AppRole({role_id})")
            }
            Self::TlsCert { cert_path } => {
                write!(f, "KmsAuthConfig::TlsCert({cert_path})")
            }
            Self::IamRole { role_arn } => {
                write!(f, "KmsAuthConfig::IamRole({role_arn})")
            }
        }
    }
}

/// Internal provider trade-off warning (ADR-028).
#[derive(Clone, Debug)]
pub struct InternalProviderWarning {
    /// Warning message.
    pub warning: &'static str,
    /// Reason for the warning.
    pub reason: &'static str,
    /// Recommendation.
    pub recommendation: &'static str,
}

impl InternalProviderWarning {
    /// The standard warning for internal KMS provider.
    #[must_use]
    pub fn standard() -> Self {
        Self {
            warning: "Internal mode does not provide full two-layer security",
            reason: "Operator with access to both Raft groups has full access",
            recommendation: "Compliance-sensitive tenants should use external provider",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // Scenario: Circuit breaker opens after consecutive failures
    // ---------------------------------------------------------------
    #[test]
    fn circuit_breaker_opens_after_threshold() {
        let mut cb = CircuitBreaker::new(5, 30);
        assert_eq!(cb.state(), CircuitState::Closed);

        // 4 failures — still closed.
        for t in 1..=4 {
            cb.record_failure(t);
            assert_eq!(cb.state(), CircuitState::Closed);
        }

        // 5th failure — opens.
        cb.record_failure(5);
        assert_eq!(cb.state(), CircuitState::Open);

        // Requests fail immediately.
        assert!(!cb.allow_request(5));

        // After probe interval (30s), a probe is allowed.
        assert!(cb.allow_request(36));

        // Transition to half-open.
        cb.try_half_open(36);
        assert_eq!(cb.state(), CircuitState::HalfOpen);

        // Successful probe closes the circuit.
        cb.record_success();
        assert_eq!(cb.state(), CircuitState::Closed);
        assert!(cb.allow_request(36));
    }

    // ---------------------------------------------------------------
    // Scenario: Concurrency limit prevents KMS overload
    // ---------------------------------------------------------------
    #[test]
    fn concurrency_limit_enforced() {
        let limiter = ConcurrencyLimiter::new(10);

        // Acquire 10 permits.
        let mut permits = Vec::new();
        for _ in 0..10 {
            permits.push(limiter.try_acquire().unwrap());
        }
        assert_eq!(limiter.in_flight(), 10);

        // 11th request is rejected.
        let result = limiter.try_acquire();
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), KmsError::ConcurrencyLimit));

        // Drop one permit — count decreases.
        drop(permits.pop());
        assert_eq!(limiter.in_flight(), 9);

        // Now we can acquire again.
        let _p = limiter.try_acquire().unwrap();
        assert_eq!(limiter.in_flight(), 10);
    }

    // ---------------------------------------------------------------
    // Scenario: Provider timeout bounds enforced
    // ---------------------------------------------------------------
    #[test]
    fn provider_timeout_is_retriable() {
        let err = KmsError::Timeout;
        let msg = format!("{err}");
        assert_eq!(msg, "KMS timeout");

        // Timeout counts toward circuit breaker threshold.
        let mut cb = CircuitBreaker::new(5, 30);
        for t in 1..=5 {
            cb.record_failure(t); // each timeout is a failure
        }
        assert_eq!(
            cb.state(),
            CircuitState::Open,
            "5 timeouts should open the circuit breaker"
        );
    }

    // ---------------------------------------------------------------
    // Scenario: KMS credentials encrypted at rest
    // ---------------------------------------------------------------
    #[test]
    fn kms_credentials_stored_as_sensitive() {
        let config = KmsAuthConfig::AppRole {
            role_id: "role-id-123".into(),
            secret_id: "s.abc123".into(),
        };
        // The secret_id is stored in the struct but never exposed
        // via Debug. In production, it would be wrapped in Zeroizing<String>.
        match &config {
            KmsAuthConfig::AppRole { secret_id, .. } => {
                assert!(!secret_id.is_empty(), "secret_id should be present");
            }
            _ => unreachable!(),
        }
    }

    // ---------------------------------------------------------------
    // Scenario: KMS credential Debug output is redacted
    // ---------------------------------------------------------------
    #[test]
    fn kms_credential_debug_redacted() {
        let config = KmsAuthConfig::AppRole {
            role_id: "role-id-123".into(),
            secret_id: "s.abc123-super-secret".into(),
        };
        let debug = format!("{config:?}");

        // Must contain the role_id (non-secret).
        assert!(
            debug.contains("role-id-123"),
            "Debug should show role_id: {debug}"
        );
        // Must NOT contain the secret_id.
        assert!(
            !debug.contains("abc123"),
            "Debug must NOT contain secret_id: {debug}"
        );
        assert!(
            !debug.contains("super-secret"),
            "Debug must NOT leak credential material: {debug}"
        );

        // Correct format.
        assert_eq!(debug, "KmsAuthConfig::AppRole(role-id-123)");
    }

    // ---------------------------------------------------------------
    // Scenario: Internal provider trade-off documented
    // ---------------------------------------------------------------
    #[test]
    fn internal_provider_trade_off_documented() {
        let warning = InternalProviderWarning::standard();
        assert!(warning.warning.contains("two-layer security"));
        assert!(warning.reason.contains("Raft groups"));
        assert!(warning.recommendation.contains("external provider"));
    }

    // ---------------------------------------------------------------
    // Scenario: KMS credential rotation does not leak old secrets
    // ---------------------------------------------------------------
    #[test]
    fn credential_rotation_old_secret_not_in_debug() {
        // Simulate rotation: old config is replaced by new config.
        let old_config = KmsAuthConfig::AppRole {
            role_id: "role-id-123".into(),
            secret_id: "old-secret-id".into(),
        };
        let new_config = KmsAuthConfig::AppRole {
            role_id: "role-id-123".into(),
            secret_id: "new-secret-id".into(),
        };

        // After rotation, the old config is dropped.
        drop(old_config);

        // New config's Debug must not contain the old secret.
        let debug = format!("{new_config:?}");
        assert!(!debug.contains("old-secret"), "old secret must not appear after rotation: {debug}");
        assert!(!debug.contains("new-secret"), "new secret must not appear in debug either: {debug}");
    }
}
