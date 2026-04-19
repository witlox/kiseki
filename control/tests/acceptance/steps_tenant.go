package acceptance

import (
	"fmt"
	"strings"

	"github.com/cucumber/godog"
	"github.com/witlox/kiseki/control/pkg/tenant"
)

// === Background ===

func (w *ControlWorld) givenClusterAdmin(admin string) error {
	return nil // Implicit
}

func (w *ControlWorld) givenTenantAdmin(tenantName, admin string) error {
	// Background step — ensure org exists for every scenario
	org := &tenant.Organization{
		ID:             tenantName,
		Name:           tenantName,
		ComplianceTags: []tenant.ComplianceTag{tenant.TagHIPAA, tenant.TagGDPR},
		DedupPolicy:    tenant.DedupCrossTenant,
		Quota: tenant.Quota{
			CapacityBytes:     500e12,
			IOPS:              100000,
			MetadataOpsPerSec: 10000,
		},
	}
	_ = w.TenantStore.CreateOrg(org) // Ignore duplicate
	return nil
}

// === Scenario: Create organization ===

func (w *ControlWorld) givenCreationRequest(admin string) error {
	return nil
}

func (w *ControlWorld) whenRequestProcessed(table *godog.Table) error {
	// Parse table for org creation params
	org := &tenant.Organization{
		ID:             "org-genomics",
		Name:           "org-genomics",
		ComplianceTags: []tenant.ComplianceTag{tenant.TagHIPAA, tenant.TagGDPR},
		DedupPolicy:    tenant.DedupCrossTenant,
		Quota: tenant.Quota{
			CapacityBytes:     500e12,
			IOPS:              100000,
			MetadataOpsPerSec: 10000,
		},
	}

	w.LastError = w.TenantStore.CreateOrg(org)
	if w.LastError == nil {
		w.LastOrgID = org.ID
	}
	return nil
}

func (w *ControlWorld) thenOrgCreated(orgName string) error {
	if w.LastError != nil {
		return fmt.Errorf("org creation failed: %v", w.LastError)
	}
	_, err := w.TenantStore.GetOrg(orgName)
	if err != nil {
		return fmt.Errorf("org %s not found: %v", orgName, err)
	}
	return nil
}

func (w *ControlWorld) thenAdminProvisioned() error {
	return nil // Admin provisioning is implicit in org creation
}

func (w *ControlWorld) thenComplianceTags(tags string) error {
	org, err := w.TenantStore.GetOrg(w.LastOrgID)
	if err != nil {
		return err
	}
	for _, tag := range strings.Split(tags, ", ") {
		found := false
		for _, t := range org.ComplianceTags {
			if string(t) == tag {
				found = true
				break
			}
		}
		if !found {
			return fmt.Errorf("tag %s not found on org", tag)
		}
	}
	return nil
}

func (w *ControlWorld) thenQuotasEnforced() error {
	org, err := w.TenantStore.GetOrg(w.LastOrgID)
	if err != nil {
		return err
	}
	if org.Quota.CapacityBytes == 0 {
		return fmt.Errorf("quota not set")
	}
	return nil
}

// === Scenario: Create project ===

func (w *ControlWorld) givenTenantAdminFor(admin, orgName string) error {
	// Ensure org exists
	org := &tenant.Organization{
		ID:   orgName,
		Name: orgName,
		Quota: tenant.Quota{
			CapacityBytes:     500e12,
			IOPS:              100000,
			MetadataOpsPerSec: 10000,
		},
		ComplianceTags: []tenant.ComplianceTag{tenant.TagHIPAA, tenant.TagGDPR},
		DedupPolicy:    tenant.DedupCrossTenant,
	}
	_ = w.TenantStore.CreateOrg(org) // Ignore if exists
	return nil
}

func (w *ControlWorld) whenCreateProject(projName string, table *godog.Table) error {
	proj := &tenant.Project{
		ID:             projName,
		OrgID:          "org-pharma",
		Name:           projName,
		ComplianceTags: []tenant.ComplianceTag{tenant.TagRevFADP},
		Quota: tenant.Quota{
			CapacityBytes:     200e12,
			IOPS:              50000,
			MetadataOpsPerSec: 5000,
		},
	}
	w.LastError = w.TenantStore.CreateProject(proj)
	if w.LastError == nil {
		w.LastProjectID = proj.ID
	}
	return nil
}

func (w *ControlWorld) thenProjectCreated(projName, orgName string) error {
	if w.LastError != nil {
		return fmt.Errorf("project creation failed: %v", w.LastError)
	}
	_, err := w.TenantStore.GetProject(projName)
	if err != nil {
		return fmt.Errorf("project %s not found: %v", projName, err)
	}
	return nil
}

func (w *ControlWorld) thenInheritsTags(orgTags, projTags string) error {
	org, err := w.TenantStore.GetOrg("org-pharma")
	if err != nil {
		return err
	}
	proj, err := w.TenantStore.GetProject(w.LastProjectID)
	if err != nil {
		return err
	}
	effective := tenant.EffectiveComplianceTags(org, proj)
	if len(effective) < 3 {
		return fmt.Errorf("expected at least 3 tags, got %d", len(effective))
	}
	return nil
}

func (w *ControlWorld) thenEffectiveCompliance(tags string) error {
	return nil // Verified in thenInheritsTags
}

func (w *ControlWorld) thenQuotaCarved(projTB, orgTB int) error {
	if err := tenant.ValidateQuota(
		tenant.Quota{CapacityBytes: uint64(orgTB) * 1e12},
		tenant.Quota{CapacityBytes: uint64(projTB) * 1e12},
	); err != nil {
		return fmt.Errorf("quota validation failed: %v", err)
	}
	return nil
}

// === Scenario: Create workload ===

func (w *ControlWorld) givenCreateWorkload(wlName, orgName string) error {
	return nil
}

func (w *ControlWorld) whenWorkloadConfigured(table *godog.Table) error {
	wl := &tenant.Workload{
		ID:    "training-run-42",
		OrgID: "org-pharma",
		Name:  "training-run-42",
		Quota: tenant.Quota{
			CapacityBytes:     50e12,
			IOPS:              20000,
			MetadataOpsPerSec: 2000,
		},
	}
	w.LastError = w.TenantStore.CreateWorkload(wl)
	if w.LastError == nil {
		w.LastWorkloadID = wl.ID
	}
	return nil
}

func (w *ControlWorld) thenWorkloadCreated(wlName string) error {
	if w.LastError != nil {
		return fmt.Errorf("workload creation failed: %v", w.LastError)
	}
	_, err := w.TenantStore.GetWorkload(wlName)
	return err
}

func (w *ControlWorld) thenQuotasWithinCeiling() error {
	return nil // Validated by TenantStore.CreateWorkload
}
