package policy

import (
	"testing"

	"github.com/witlox/kiseki/control/pkg/tenant"
)

func TestEffectiveStalenessHIPAA(t *testing.T) {
	tags := []tenant.ComplianceTag{tenant.TagHIPAA}

	// View wants 1s but HIPAA floor is 2s.
	if got := EffectiveStaleness(tags, 1000); got != 2000 {
		t.Errorf("expected 2000, got %d", got)
	}

	// View wants 5s — above floor, so preference wins.
	if got := EffectiveStaleness(tags, 5000); got != 5000 {
		t.Errorf("expected 5000, got %d", got)
	}
}

func TestEffectiveStalenessNoTags(t *testing.T) {
	// No compliance tags → floor is 0, preference wins.
	if got := EffectiveStaleness(nil, 1000); got != 1000 {
		t.Errorf("expected 1000, got %d", got)
	}
}
