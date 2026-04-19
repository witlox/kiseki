// Package tenant provides tenant lifecycle management.
//
// Manages the three-level hierarchy: Organization → Project → Workload.
// Compliance tags attach at any level and inherit downward (union of
// constraints). Quotas are bounded by parent ceilings.
//
// Spec: ubiquitous-language.md#Tenancy-and-access, I-T1..I-T4.
package tenant

import "fmt"

// ComplianceTag represents a regulatory compliance regime.
type ComplianceTag string

const (
	// TagHIPAA indicates HIPAA §164.312 applies.
	TagHIPAA ComplianceTag = "HIPAA"
	// TagGDPR indicates GDPR applies.
	TagGDPR ComplianceTag = "GDPR"
	// TagRevFADP indicates Swiss revFADP applies.
	TagRevFADP ComplianceTag = "revFADP"
	// TagSwissResidency constrains data residency to Switzerland.
	TagSwissResidency ComplianceTag = "SwissResidency"
)

// DedupPolicy determines chunk ID derivation per tenant.
type DedupPolicy string

const (
	// DedupCrossTenant uses sha256(plaintext) for cross-tenant dedup.
	DedupCrossTenant DedupPolicy = "cross-tenant"
	// DedupTenantIsolated uses HMAC(plaintext, tenant_key) — no cross-tenant dedup.
	DedupTenantIsolated DedupPolicy = "tenant-isolated"
)

// Quota defines resource limits at a tenant hierarchy level.
type Quota struct {
	CapacityBytes     uint64
	IOPS              uint64
	MetadataOpsPerSec uint64
}

// Organization is the top-level tenant (I-T1, I-T3).
type Organization struct {
	ID             string
	Name           string
	ComplianceTags []ComplianceTag
	DedupPolicy    DedupPolicy
	Quota          Quota
}

// Project is an optional grouping within an organization.
type Project struct {
	ID             string
	OrgID          string
	Name           string
	ComplianceTags []ComplianceTag // merged with org tags
	Quota          Quota           // bounded by org quota
}

// Workload is the runtime isolation unit within a tenant.
type Workload struct {
	ID     string
	OrgID  string
	ProjID string // empty if no project
	Name   string
	Quota  Quota
}

// EffectiveComplianceTags returns the union of compliance tags from
// org, project, and any additional tags (I-K9: union of constraints,
// tags cannot weaken inherited policy).
func EffectiveComplianceTags(org *Organization, proj *Project) []ComplianceTag {
	seen := make(map[ComplianceTag]bool)
	var result []ComplianceTag

	for _, t := range org.ComplianceTags {
		if !seen[t] {
			seen[t] = true
			result = append(result, t)
		}
	}

	if proj != nil {
		for _, t := range proj.ComplianceTags {
			if !seen[t] {
				seen[t] = true
				result = append(result, t)
			}
		}
	}

	return result
}

// ValidateQuota checks that a child quota does not exceed the parent ceiling.
func ValidateQuota(parent, child Quota) error {
	if child.CapacityBytes > parent.CapacityBytes {
		return fmt.Errorf("capacity %d exceeds parent ceiling %d", child.CapacityBytes, parent.CapacityBytes)
	}
	if child.IOPS > parent.IOPS {
		return fmt.Errorf("IOPS %d exceeds parent ceiling %d", child.IOPS, parent.IOPS)
	}
	if child.MetadataOpsPerSec > parent.MetadataOpsPerSec {
		return fmt.Errorf("metadata ops/sec %d exceeds parent ceiling %d", child.MetadataOpsPerSec, parent.MetadataOpsPerSec)
	}
	return nil
}
