package tenant

import "testing"

func TestEffectiveComplianceTags(t *testing.T) {
	org := &Organization{
		ComplianceTags: []ComplianceTag{TagHIPAA, TagGDPR},
	}
	proj := &Project{
		ComplianceTags: []ComplianceTag{TagRevFADP, TagHIPAA}, // HIPAA is duplicate
	}

	tags := EffectiveComplianceTags(org, proj)

	// Should have 3 unique tags: HIPAA, GDPR, revFADP.
	if len(tags) != 3 {
		t.Errorf("expected 3 tags, got %d: %v", len(tags), tags)
	}

	seen := make(map[ComplianceTag]bool)
	for _, tag := range tags {
		seen[tag] = true
	}
	for _, expected := range []ComplianceTag{TagHIPAA, TagGDPR, TagRevFADP} {
		if !seen[expected] {
			t.Errorf("missing tag: %s", expected)
		}
	}
}

func TestEffectiveComplianceTagsNoProject(t *testing.T) {
	org := &Organization{
		ComplianceTags: []ComplianceTag{TagGDPR},
	}

	tags := EffectiveComplianceTags(org, nil)
	if len(tags) != 1 || tags[0] != TagGDPR {
		t.Errorf("expected [GDPR], got %v", tags)
	}
}

func TestValidateQuota(t *testing.T) {
	parent := Quota{CapacityBytes: 1000, IOPS: 100, MetadataOpsPerSec: 50}

	// Valid child.
	if err := ValidateQuota(parent, Quota{CapacityBytes: 500, IOPS: 50, MetadataOpsPerSec: 25}); err != nil {
		t.Errorf("expected valid, got: %v", err)
	}

	// Exceeds capacity.
	if err := ValidateQuota(parent, Quota{CapacityBytes: 2000, IOPS: 50, MetadataOpsPerSec: 25}); err == nil {
		t.Error("expected error for capacity overflow")
	}

	// Exceeds IOPS.
	if err := ValidateQuota(parent, Quota{CapacityBytes: 500, IOPS: 200, MetadataOpsPerSec: 25}); err == nil {
		t.Error("expected error for IOPS overflow")
	}
}
