package iam

import (
	"testing"
	"time"
)

func TestAccessRequestLifecycle(t *testing.T) {
	req := &AccessRequest{
		ID:            "req-1",
		RequesterID:   "admin-ops",
		TenantID:      "org-pharma",
		Scope:         ScopeNamespace,
		ScopeTarget:   "trials",
		AccessLevel:   AccessReadOnly,
		DurationHours: 4,
		Status:        StatusPending,
		RequestedAt:   time.Now(),
	}

	// Pending → not active.
	if req.IsActive() {
		t.Error("pending request should not be active")
	}

	// Approve.
	if err := req.Approve(); err != nil {
		t.Fatalf("approve failed: %v", err)
	}
	if req.Status != StatusApproved {
		t.Errorf("expected approved, got %s", req.Status)
	}
	if !req.IsActive() {
		t.Error("approved request should be active")
	}

	// Double-approve fails.
	if err := req.Approve(); err == nil {
		t.Error("expected error on double approve")
	}
}

func TestAccessRequestDeny(t *testing.T) {
	req := &AccessRequest{
		ID:     "req-2",
		Status: StatusPending,
	}

	if err := req.Deny(); err != nil {
		t.Fatalf("deny failed: %v", err)
	}
	if req.Status != StatusDenied {
		t.Errorf("expected denied, got %s", req.Status)
	}
	if req.IsActive() {
		t.Error("denied request should not be active")
	}
}

func TestAccessRequestExpiry(t *testing.T) {
	req := &AccessRequest{
		ID:            "req-3",
		Status:        StatusApproved,
		DurationHours: 0, // expires immediately
		ExpiresAt:     time.Now().Add(-1 * time.Minute),
	}

	req.CheckExpired()
	if req.Status != StatusExpired {
		t.Errorf("expected expired, got %s", req.Status)
	}
}
