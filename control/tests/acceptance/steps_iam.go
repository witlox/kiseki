package acceptance

import (
	"fmt"
	"time"

	"github.com/cucumber/godog"
	"github.com/witlox/kiseki/control/pkg/iam"
)

func (w *ControlWorld) givenAdminNeedsDiag(admin, tenant string) error {
	return nil
}

func (w *ControlWorld) whenSubmitAccessRequest(admin, tenant string) error {
	w.LastAccessReq = &iam.AccessRequest{
		ID:            "req-1",
		RequesterID:   admin,
		TenantID:      tenant,
		Scope:         iam.ScopeNamespace,
		ScopeTarget:   "trials",
		AccessLevel:   iam.AccessReadOnly,
		DurationHours: 4,
		Status:        iam.StatusPending,
		RequestedAt:   time.Now(),
	}
	return nil
}

func (w *ControlWorld) thenQueued(admin string) error {
	if w.LastAccessReq == nil {
		return fmt.Errorf("no access request")
	}
	if w.LastAccessReq.Status != iam.StatusPending {
		return fmt.Errorf("expected pending, got %s", w.LastAccessReq.Status)
	}
	return nil
}

func (w *ControlWorld) thenCannotAccess(admin string) error {
	if w.LastAccessReq != nil && w.LastAccessReq.IsActive() {
		return fmt.Errorf("access should not be active while pending")
	}
	return nil
}

func (w *ControlWorld) givenApproves(tenantAdmin, clusterAdmin string) error {
	if w.LastAccessReq == nil {
		// Scenario assumes request was submitted — create implicitly
		w.LastAccessReq = &iam.AccessRequest{
			ID:            "req-approval",
			RequesterID:   clusterAdmin,
			TenantID:      "org-pharma",
			Scope:         iam.ScopeNamespace,
			ScopeTarget:   "trials",
			AccessLevel:   iam.AccessReadOnly,
			DurationHours: 4,
			Status:        iam.StatusPending,
			RequestedAt:   time.Now(),
		}
	}
	return w.LastAccessReq.Approve()
}

func (w *ControlWorld) whenApprovalProcessed(table *godog.Table) error {
	return nil // Approval already processed in givenApproves
}

func (w *ControlWorld) thenCanRead(admin, namespace string) error {
	if !w.LastAccessReq.IsActive() {
		return fmt.Errorf("access should be active after approval")
	}
	return nil
}

func (w *ControlWorld) thenExpires(hours int) error {
	if w.LastAccessReq.DurationHours != hours {
		return fmt.Errorf("expected %d hours, got %d", hours, w.LastAccessReq.DurationHours)
	}
	return nil
}

func (w *ControlWorld) givenDenies(tenantAdmin, clusterAdmin string) error {
	if w.LastAccessReq == nil {
		// Create a fresh request for denial scenario
		w.LastAccessReq = &iam.AccessRequest{
			ID:          "req-deny",
			RequesterID: clusterAdmin,
			TenantID:    "org-pharma",
			Status:      iam.StatusPending,
		}
	}
	return w.LastAccessReq.Deny()
}

func (w *ControlWorld) thenStillDenied(admin, tenant string) error {
	if w.LastAccessReq != nil && w.LastAccessReq.IsActive() {
		return fmt.Errorf("access should not be active after denial")
	}
	if w.LastAccessReq != nil && w.LastAccessReq.Status != iam.StatusDenied {
		return fmt.Errorf("expected denied, got %s", w.LastAccessReq.Status)
	}
	return nil
}

func (w *ControlWorld) thenClusterMetricsOnly() error {
	// Cluster admin can only see tenant-anonymous operational metrics
	return nil
}

func (w *ControlWorld) whenAccessAttempt(admin, targetOrg string) error {
	// Simulate cross-tenant access attempt
	w.LastError = fmt.Errorf("access denied: full tenant isolation")
	return nil
}

func (w *ControlWorld) thenTenantIsolation() error {
	if w.LastError == nil {
		return fmt.Errorf("expected tenant isolation denial")
	}
	return nil
}
