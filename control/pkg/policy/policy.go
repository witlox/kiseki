// Package policy provides quota enforcement and compliance tag evaluation.
//
// Spec: I-T2 (quota enforcement), I-K9 (compliance floor on staleness).
package policy

import "github.com/witlox/kiseki/control/pkg/tenant"

// EffectiveStaleness computes the effective staleness bound for a view,
// given the compliance tags and the view descriptor's preference.
//
// The effective bound is max(view_preference, compliance_floor) — compliance
// tags set a non-overridable floor (I-K9).
func EffectiveStaleness(tags []tenant.ComplianceTag, viewPreferenceMs uint64) uint64 {
	floor := complianceFloorMs(tags)
	if viewPreferenceMs > floor {
		return viewPreferenceMs
	}
	return floor
}

// complianceFloorMs returns the minimum staleness bound implied by
// the compliance tags. HIPAA has a 2-second floor.
func complianceFloorMs(tags []tenant.ComplianceTag) uint64 {
	var floor uint64
	for _, tag := range tags {
		switch tag {
		case tenant.TagHIPAA:
			// HIPAA §164.312 requires near-real-time audit — 2s floor.
			if floor < 2000 {
				floor = 2000
			}
		case tenant.TagGDPR, tenant.TagRevFADP:
			// No specific staleness requirement beyond HIPAA.
		default:
			// Custom tags: no floor adjustment.
		}
	}
	return floor
}
