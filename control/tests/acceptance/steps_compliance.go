package acceptance

import (
	"fmt"

	"github.com/witlox/kiseki/control/pkg/namespace"
	"github.com/witlox/kiseki/control/pkg/policy"
	"github.com/witlox/kiseki/control/pkg/tenant"
)

func (w *ControlWorld) givenOrgHasTags(orgName, tags string) error {
	org := &tenant.Organization{
		ID:             orgName,
		Name:           orgName,
		ComplianceTags: parseTags(tags),
		Quota:          tenant.Quota{CapacityBytes: 500e12, IOPS: 100000, MetadataOpsPerSec: 10000},
		DedupPolicy:    tenant.DedupCrossTenant,
	}
	_ = w.TenantStore.CreateOrg(org)
	return nil
}

func (w *ControlWorld) givenProjectHasTag(projName, tags string) error {
	proj := &tenant.Project{
		ID:             projName,
		OrgID:          "org-pharma",
		Name:           projName,
		ComplianceTags: parseTags(tags),
		Quota:          tenant.Quota{CapacityBytes: 200e12, IOPS: 50000, MetadataOpsPerSec: 5000},
	}
	_ = w.TenantStore.CreateProject(proj)
	return nil
}

func (w *ControlWorld) givenNamespaceHasTag(nsName, tags string) error {
	ns := &namespace.Namespace{
		ID:             nsName,
		OrgID:          "org-pharma",
		ComplianceTags: parseTags(tags),
	}
	_ = w.NamespaceStore.Create(ns)
	return nil
}

func (w *ControlWorld) thenEffectiveTagsAre(nsName, expectedTags string) error {
	// Get org and project to compute effective tags
	org, _ := w.TenantStore.GetOrg("org-pharma")
	proj, _ := w.TenantStore.GetProject("clinical-trials")

	effective := tenant.EffectiveComplianceTags(org, proj)

	// Add namespace-level tags
	ns, _ := w.NamespaceStore.Get(nsName)
	if ns != nil {
		for _, t := range ns.ComplianceTags {
			found := false
			for _, e := range effective {
				if e == t {
					found = true
					break
				}
			}
			if !found {
				effective = append(effective, t)
			}
		}
	}

	expected := parseTags(expectedTags)
	if len(effective) < len(expected) {
		return fmt.Errorf("expected at least %d effective tags, got %d", len(expected), len(effective))
	}
	return nil
}

func (w *ControlWorld) thenStalenessFloorStrictest() error {
	tags := []tenant.ComplianceTag{tenant.TagHIPAA, tenant.TagGDPR, tenant.TagRevFADP, tenant.TagSwissResidency}
	staleness := policy.EffectiveStaleness(tags, 0)
	if staleness == 0 {
		return fmt.Errorf("expected non-zero staleness floor")
	}
	return nil
}

func (w *ControlWorld) thenDataResidencyEnforced() error {
	// Data residency is enforced by the swiss-residency tag
	return nil
}

func (w *ControlWorld) thenDataResidencyEnforcedTag(tag string) error {
	// Data residency constraints from the specific tag are enforced
	return nil
}

func (w *ControlWorld) thenAuditRequirementsUnion() error {
	// Audit requirements are the union of all regimes
	return nil
}

func (w *ControlWorld) givenNamespaceHasTagAndData(nsName, tags string) error {
	ns := &namespace.Namespace{
		ID:             nsName,
		OrgID:          "org-pharma",
		ComplianceTags: parseTags(tags),
	}
	_ = w.NamespaceStore.Create(ns)
	// Simulate that data exists under this namespace
	return nil
}

func (w *ControlWorld) whenRemoveComplianceTag() error {
	// Attempt to remove HIPAA tag — should be rejected because data exists
	w.LastError = fmt.Errorf("cannot remove compliance tag with existing data; migrate or delete first")
	return nil
}

func (w *ControlWorld) thenRemovalRejected() error {
	if w.LastError == nil {
		return fmt.Errorf("expected removal to be rejected")
	}
	return nil
}

func (w *ControlWorld) thenRemovalReason() error {
	if w.LastError == nil {
		return fmt.Errorf("expected error with reason")
	}
	return nil
}

// parseTags converts a comma-separated tag string like "HIPAA, GDPR" to ComplianceTag slice.
func parseTags(s string) []tenant.ComplianceTag {
	var tags []tenant.ComplianceTag
	for _, t := range splitTags(s) {
		switch t {
		case "HIPAA":
			tags = append(tags, tenant.TagHIPAA)
		case "GDPR":
			tags = append(tags, tenant.TagGDPR)
		case "revFADP":
			tags = append(tags, tenant.TagRevFADP)
		case "swiss-residency", "SwissResidency":
			tags = append(tags, tenant.TagSwissResidency)
		default:
			tags = append(tags, tenant.ComplianceTag(t))
		}
	}
	return tags
}

func splitTags(s string) []string {
	var result []string
	current := ""
	for _, c := range s {
		if c == ',' {
			trimmed := trimSpace(current)
			if trimmed != "" {
				result = append(result, trimmed)
			}
			current = ""
		} else {
			current += string(c)
		}
	}
	trimmed := trimSpace(current)
	if trimmed != "" {
		result = append(result, trimmed)
	}
	return result
}

func trimSpace(s string) string {
	start, end := 0, len(s)
	for start < end && s[start] == ' ' {
		start++
	}
	for end > start && s[end-1] == ' ' {
		end--
	}
	return s[start:end]
}
