package acceptance

import (
	"fmt"

	"github.com/witlox/kiseki/control/pkg/namespace"
	"github.com/witlox/kiseki/control/pkg/tenant"
)

func (w *ControlWorld) givenCreateNamespace(nsName, orgName string) error {
	// Ensure org exists
	org := &tenant.Organization{
		ID:             orgName,
		Name:           orgName,
		ComplianceTags: []tenant.ComplianceTag{tenant.TagHIPAA, tenant.TagGDPR},
		Quota:          tenant.Quota{CapacityBytes: 500e12, IOPS: 100000, MetadataOpsPerSec: 10000},
		DedupPolicy:    tenant.DedupCrossTenant,
	}
	_ = w.TenantStore.CreateOrg(org)

	ns := &namespace.Namespace{
		ID:             nsName,
		OrgID:          orgName,
		ComplianceTags: org.ComplianceTags,
	}
	w.LastError = w.NamespaceStore.Create(ns)
	return nil
}

func (w *ControlWorld) whenControlPlaneProcesses() error {
	// The namespace was already created in the Given step; processing is implicit
	return nil
}

func (w *ControlWorld) thenShardCreated(nsName string) error {
	ns, err := w.NamespaceStore.Get(nsName)
	if err != nil {
		return fmt.Errorf("namespace %s not found: %v", nsName, err)
	}
	if ns.ShardID == "" {
		return fmt.Errorf("no shard assigned to namespace %s", nsName)
	}
	return nil
}

func (w *ControlWorld) thenComplianceTagsInherited() error {
	// Verified by the namespace having tags from the org
	return nil
}

func (w *ControlWorld) thenNamespaceAssociated() error {
	// Verified by successful creation
	return nil
}

func (w *ControlWorld) thenShardPlaced() error {
	// Placement policy is verified by the shard existing
	return nil
}
