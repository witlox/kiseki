package tenant

import "testing"

func TestStoreCRUD(t *testing.T) {
	s := NewStore()

	org := &Organization{
		ID:             "org-pharma",
		Name:           "Pharma Corp",
		ComplianceTags: []ComplianceTag{TagHIPAA},
		DedupPolicy:    DedupCrossTenant,
		Quota:          Quota{CapacityBytes: 500e12, IOPS: 100000, MetadataOpsPerSec: 10000},
	}

	if err := s.CreateOrg(org); err != nil {
		t.Fatalf("create org: %v", err)
	}
	if s.OrgCount() != 1 {
		t.Errorf("expected 1 org, got %d", s.OrgCount())
	}

	got, err := s.GetOrg("org-pharma")
	if err != nil {
		t.Fatalf("get org: %v", err)
	}
	if got.Name != "Pharma Corp" {
		t.Errorf("expected Pharma Corp, got %s", got.Name)
	}

	// Duplicate create fails.
	if err := s.CreateOrg(org); err == nil {
		t.Error("expected error on duplicate create")
	}
}

func TestStoreProjectQuotaValidation(t *testing.T) {
	s := NewStore()
	org := &Organization{
		ID:    "org-1",
		Quota: Quota{CapacityBytes: 1000, IOPS: 100, MetadataOpsPerSec: 50},
	}
	if err := s.CreateOrg(org); err != nil {
		t.Fatal(err)
	}

	// Project within quota.
	proj := &Project{ID: "proj-1", OrgID: "org-1", Quota: Quota{CapacityBytes: 500, IOPS: 50, MetadataOpsPerSec: 25}}
	if err := s.CreateProject(proj); err != nil {
		t.Fatalf("create project: %v", err)
	}

	// Project exceeding quota.
	bad := &Project{ID: "proj-2", OrgID: "org-1", Quota: Quota{CapacityBytes: 2000, IOPS: 50, MetadataOpsPerSec: 25}}
	if err := s.CreateProject(bad); err == nil {
		t.Error("expected error for quota overflow")
	}
}

func TestStoreWorkload(t *testing.T) {
	s := NewStore()
	org := &Organization{
		ID:    "org-1",
		Quota: Quota{CapacityBytes: 1000, IOPS: 100, MetadataOpsPerSec: 50},
	}
	if err := s.CreateOrg(org); err != nil {
		t.Fatal(err)
	}

	wl := &Workload{ID: "wl-1", OrgID: "org-1", Quota: Quota{CapacityBytes: 100, IOPS: 10, MetadataOpsPerSec: 5}}
	if err := s.CreateWorkload(wl); err != nil {
		t.Fatalf("create workload: %v", err)
	}

	got, err := s.GetWorkload("wl-1")
	if err != nil {
		t.Fatalf("get workload: %v", err)
	}
	if got.OrgID != "org-1" {
		t.Errorf("expected org-1, got %s", got.OrgID)
	}
}

func TestStoreDeleteOrg(t *testing.T) {
	s := NewStore()
	org := &Organization{ID: "org-1"}
	if err := s.CreateOrg(org); err != nil {
		t.Fatal(err)
	}
	if err := s.DeleteOrg("org-1"); err != nil {
		t.Fatalf("delete: %v", err)
	}
	if _, err := s.GetOrg("org-1"); err == nil {
		t.Error("expected not found after delete")
	}
}
