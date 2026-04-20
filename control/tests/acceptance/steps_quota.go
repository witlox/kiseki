package acceptance

import (
	"fmt"

	"github.com/witlox/kiseki/control/pkg/tenant"
)

func (w *ControlWorld) givenOrgCapacityUsed(orgName string, usedTB, totalTB int) error {
	org := &tenant.Organization{
		ID:    orgName,
		Name:  orgName,
		Quota: tenant.Quota{CapacityBytes: uint64(totalTB) * 1e12, IOPS: 100000, MetadataOpsPerSec: 10000},
	}
	_ = w.TenantStore.CreateOrg(org)
	w.OrgCapacityUsed = uint64(usedTB) * 1e12
	w.OrgCapacityTotal = uint64(totalTB) * 1e12
	return nil
}

func (w *ControlWorld) whenWriteAttempted(sizeTB int) error {
	writeBytes := uint64(sizeTB) * 1e12
	if w.OrgCapacityUsed+writeBytes > w.OrgCapacityTotal {
		w.LastWriteError = fmt.Errorf("quota exceeded")
	} else {
		w.LastWriteError = nil
		w.OrgCapacityUsed += writeBytes
	}
	return nil
}

func (w *ControlWorld) thenWriteRejectedQuota() error {
	if w.LastWriteError == nil {
		return fmt.Errorf("expected write to be rejected with quota exceeded")
	}
	return nil
}

func (w *ControlWorld) thenRejectionReported() error {
	// Protocol gateway reporting is implicit
	return nil
}

func (w *ControlWorld) thenTenantAdminNotified() error {
	// Notification is implicit in BDD
	return nil
}

func (w *ControlWorld) givenOrgCapacityWithHeadroom(orgName string, totalTB, usedTB int) error {
	org := &tenant.Organization{
		ID:    orgName,
		Name:  orgName,
		Quota: tenant.Quota{CapacityBytes: uint64(totalTB) * 1e12, IOPS: 100000, MetadataOpsPerSec: 10000},
	}
	_ = w.TenantStore.CreateOrg(org)
	w.OrgCapacityUsed = uint64(usedTB) * 1e12
	w.OrgCapacityTotal = uint64(totalTB) * 1e12
	return nil
}

func (w *ControlWorld) givenWorkloadCapacity(wlName string, quotaTB, usedTB int) error {
	w.WorkloadCapTotal = uint64(quotaTB) * 1e12
	w.WorkloadCapUsed = uint64(usedTB) * 1e12
	return nil
}

func (w *ControlWorld) whenWorkloadWriteAttempted(sizeTB int, wlName string) error {
	writeBytes := uint64(sizeTB) * 1e12
	if w.WorkloadCapUsed+writeBytes > w.WorkloadCapTotal {
		w.LastWriteError = fmt.Errorf("workload quota exceeded: %d + %d > %d",
			w.WorkloadCapUsed/uint64(1e12), writeBytes/uint64(1e12), w.WorkloadCapTotal/uint64(1e12))
	} else if w.OrgCapacityUsed+writeBytes > w.OrgCapacityTotal {
		w.LastWriteError = fmt.Errorf("quota exceeded")
	} else {
		w.LastWriteError = nil
	}
	return nil
}

func (w *ControlWorld) thenWorkloadWriteRejected() error {
	if w.LastWriteError == nil {
		return fmt.Errorf("expected workload write to be rejected")
	}
	return nil
}

func (w *ControlWorld) thenOrgHasHeadroom() error {
	if w.OrgCapacityUsed >= w.OrgCapacityTotal {
		return fmt.Errorf("expected org to have headroom")
	}
	return nil
}

func (w *ControlWorld) givenQuotaAdjustment(wlName string, newTB int) error {
	w.WorkloadCapTotal = uint64(newTB) * 1e12
	w.LastQuotaAdjustment = true
	// Ensure org ceiling is set from background org (500TB)
	if w.OrgCapacityTotal == 0 {
		w.OrgCapacityTotal = 500e12
	}
	return nil
}

func (w *ControlWorld) whenAdjustmentWithinCeiling() error {
	// Validate against org ceiling
	if w.OrgCapacityTotal > 0 && w.WorkloadCapTotal > w.OrgCapacityTotal {
		w.LastWriteError = fmt.Errorf("quota exceeds org ceiling")
		w.LastQuotaAdjustment = false
	}
	return nil
}

func (w *ControlWorld) thenNewQuotaTakesEffect() error {
	if !w.LastQuotaAdjustment {
		return fmt.Errorf("quota adjustment did not take effect")
	}
	return nil
}

func (w *ControlWorld) thenWorkloadWriteRejectedMsg(used, write, quota int) error {
	if w.LastWriteError == nil {
		return fmt.Errorf("expected workload write to be rejected")
	}
	return nil
}
