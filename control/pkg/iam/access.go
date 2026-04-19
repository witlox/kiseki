// Package iam provides identity and access management for the Kiseki
// control plane.
//
// Implements the zero-trust boundary between cluster admin and tenant
// admin (I-T4): cluster admin cannot access tenant config, logs, or
// data without explicit tenant admin approval.
//
// Spec: ubiquitous-language.md#Authentication, I-T4, I-Auth4.
package iam

import (
	"fmt"
	"time"
)

// AccessScope defines what the access request covers.
type AccessScope string

const (
	// ScopeNamespace limits access to a specific namespace.
	ScopeNamespace AccessScope = "namespace"
	// ScopeTenant allows access to the entire tenant.
	ScopeTenant AccessScope = "tenant"
)

// AccessLevel defines the permitted operations.
type AccessLevel string

const (
	// AccessReadOnly permits read operations only.
	AccessReadOnly AccessLevel = "read-only"
	// AccessReadWrite permits read and write operations.
	AccessReadWrite AccessLevel = "read-write"
)

// RequestStatus tracks the lifecycle of an access request.
type RequestStatus string

const (
	// StatusPending awaits tenant admin decision.
	StatusPending RequestStatus = "pending"
	// StatusApproved grants access for the specified duration.
	StatusApproved RequestStatus = "approved"
	// StatusDenied rejects the access request.
	StatusDenied RequestStatus = "denied"
	// StatusExpired indicates the access window has closed.
	StatusExpired RequestStatus = "expired"
)

// AccessRequest represents a cluster admin requesting access to
// tenant data (I-T4: deny by default).
type AccessRequest struct {
	ID            string
	RequesterID   string // cluster admin identity
	TenantID      string
	Scope         AccessScope
	ScopeTarget   string // namespace name, if scope=namespace
	AccessLevel   AccessLevel
	DurationHours int
	Status        RequestStatus
	RequestedAt   time.Time
	DecidedAt     time.Time
	ExpiresAt     time.Time
}

// Approve transitions the request to approved status with a time-limited
// access window.
func (r *AccessRequest) Approve() error {
	if r.Status != StatusPending {
		return fmt.Errorf("cannot approve request in status %s", r.Status)
	}
	r.Status = StatusApproved
	r.DecidedAt = time.Now()
	r.ExpiresAt = r.DecidedAt.Add(time.Duration(r.DurationHours) * time.Hour)
	return nil
}

// Deny rejects the access request.
func (r *AccessRequest) Deny() error {
	if r.Status != StatusPending {
		return fmt.Errorf("cannot deny request in status %s", r.Status)
	}
	r.Status = StatusDenied
	r.DecidedAt = time.Now()
	return nil
}

// IsActive checks whether the access grant is currently valid.
func (r *AccessRequest) IsActive() bool {
	return r.Status == StatusApproved && time.Now().Before(r.ExpiresAt)
}

// CheckExpired transitions approved requests past their window to expired.
func (r *AccessRequest) CheckExpired() {
	if r.Status == StatusApproved && time.Now().After(r.ExpiresAt) {
		r.Status = StatusExpired
	}
}
