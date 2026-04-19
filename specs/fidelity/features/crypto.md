# Fidelity: kiseki-crypto (key-management.feature)

## Scenario → Test Mapping

| # | Scenario | Test(s) | Depth | Notes |
|---|----------|---------|-------|-------|
| 1 | System DEK generation | `seal_open_roundtrip`, `hkdf_deterministic`, `hkdf_different_inputs` | THOROUGH | HKDF + AEAD round-trip verified |
| 2 | System KEK rotation | `key_rotation_creates_new_epoch` (keymanager) | MODERATE | Rotation logic in keymanager, not crypto |
| 3 | Tenant KEK wraps system DEK | `tenant_wrap_unwrap_roundtrip`, prop `tenant_wrap_unwrap_roundtrip` | THOROUGH | Full wrap→unwrap→decrypt path |
| 4 | Tenant without KEK | `missing_tenant_wrapping_fails` | THOROUGH | Error on missing wrapping |
| 5 | Epoch-based tenant key rotation | — | NONE | Rotation lifecycle not in crypto crate scope |
| 6 | Full re-encryption | — | NONE | Re-encryption orchestration not implemented |
| 7 | Crypto-shred destroys tenant KEK | `wrong_tenant_kek_fails` | SHALLOW | Wrong KEK fails, but no shred lifecycle test |
| 8 | Crypto-shred with retention hold | — | NONE | Integration with chunk store |
| 9 | Crypto-shred doesn't affect other tenants | — | NONE | Integration scenario |
| 10 | Tenant KMS unreachable — cached keys | — | NONE | Cache TTL not in crypto crate |
| 11 | Tenant KMS unreachable — cache expired | — | NONE | Cache TTL not in crypto crate |
| 12 | Tenant KMS from federated site | — | NONE | Federation not implemented |
| 13 | All key events audited | — | NONE | Audit integration |
| 14 | Tenant KMS permanently lost | — | NONE | Key manager scope |
| 15 | System key manager failure | — | NONE | Key manager scope |
| 16 | Concurrent rotation and crypto-shred | — | NONE | Orchestration scope |
| 17 | Key epoch mismatch during read | `different_epochs_different_deks` (keymanager) | MODERATE | Multi-epoch key access tested in keymanager |

## Summary

- **Scenarios with THOROUGH/MODERATE coverage**: 4 of 17 (24%)
- **Scenarios with NONE**: 11 of 17 (65%)
- **Context**: Most NONE scenarios are integration/orchestration that belong to keymanager, control plane, or integration (Phase 12). The crypto *library* primitives (AEAD, HKDF, envelope, chunk ID) have THOROUGH coverage including property tests.

## Confidence: **MEDIUM**

Crypto primitives are well-tested (26 tests, 7 property tests). But the feature file covers the full key lifecycle including orchestration scenarios that are not in scope for this crate. The primitives are correct; the orchestration is untested.
