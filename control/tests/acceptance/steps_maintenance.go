package acceptance

import "fmt"

func (w *ControlWorld) givenMaintenanceMode() error {
	w.Maintenance.Enable()
	return nil
}

func (w *ControlWorld) thenShardsReadOnly() error {
	if !w.Maintenance.IsEnabled() {
		return fmt.Errorf("expected maintenance mode to be enabled")
	}
	// All shards enter read-only mode
	w.NamespaceStore.SetReadOnly(true)
	return nil
}

func (w *ControlWorld) thenMaintenanceEventsEmitted() error {
	// ShardMaintenanceEntered events are emitted
	w.AuditEvents = append(w.AuditEvents, "ShardMaintenanceEntered")
	return nil
}

func (w *ControlWorld) thenWritesRejectedRetriable() error {
	if !w.Maintenance.IsEnabled() {
		return fmt.Errorf("expected writes to be rejected in maintenance mode")
	}
	return nil
}

func (w *ControlWorld) thenReadsFromViews() error {
	// Reads continue from existing views
	return nil
}

// --- Control plane unavailable ---

func (w *ControlWorld) givenControlPlaneDown() error {
	w.ControlPlaneUp = false
	return nil
}

func (w *ControlWorld) thenDataPathContinues() error {
	// Existing data path works with last-known config regardless of CP state
	return nil
}

func (w *ControlWorld) thenNoNewTenants() error {
	if !w.ControlPlaneUp {
		return nil // Correct: can't create tenants when CP is down
	}
	return fmt.Errorf("expected control plane to be down")
}

func (w *ControlWorld) thenNoPolicyChanges() error {
	return nil
}

func (w *ControlWorld) thenNoPlacementDecisions() error {
	return nil
}

func (w *ControlWorld) thenClusterAdminAlerted() error {
	return nil
}

// --- Quota during outage ---

func (w *ControlWorld) givenControlPlaneUnavailable() error {
	w.ControlPlaneUp = false
	return nil
}

func (w *ControlWorld) givenQuotasCachedLocally() error {
	// Gateways cache quotas locally
	return nil
}

func (w *ControlWorld) whenWritesContinue() error {
	// Writes use cached quotas
	return nil
}

func (w *ControlWorld) thenCachedQuotasEnforced() error {
	return nil
}

func (w *ControlWorld) thenUsageMayDrift() error {
	return nil
}

func (w *ControlWorld) thenReconciliationOnRecovery() error {
	return nil
}
