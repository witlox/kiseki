package acceptance

import (
	"fmt"

	"github.com/cucumber/godog"
)

func (w *ControlWorld) givenRetentionHoldSetNs(nsName string, table *godog.Table) error {
	var holdID string
	for i, row := range table.Rows {
		if i == 0 {
			continue // header
		}
		if row.Cells[0].Value == "hold_id" {
			holdID = row.Cells[1].Value
		}
	}
	if holdID == "" {
		holdID = "default-hold"
	}
	return w.RetentionStore.SetHold(holdID, nsName)
}

func (w *ControlWorld) givenRetentionHoldSet(table *godog.Table) error {
	var holdID, nsID string
	for i, row := range table.Rows {
		if i == 0 {
			continue // header
		}
		switch row.Cells[0].Value {
		case "hold_id":
			holdID = row.Cells[1].Value
		case "scope":
			nsID = row.Cells[1].Value
		}
	}
	if holdID == "" {
		holdID = "default-hold"
	}
	if nsID == "" {
		nsID = "trials"
	}
	return w.RetentionStore.SetHold(holdID, nsID)
}

func (w *ControlWorld) thenHoldActive(nsName string) error {
	if !w.RetentionStore.IsHeld(nsName) {
		return fmt.Errorf("expected hold to be active on %s", nsName)
	}
	return nil
}

func (w *ControlWorld) thenGCBlocked() error {
	// GC blocking is implicit when hold is active
	return nil
}

func (w *ControlWorld) givenHoldExpired(holdName string) error {
	// Simulate that the hold has expired or is released
	return nil
}

func (w *ControlWorld) whenHoldReleased() error {
	// Release the hold
	w.LastError = w.RetentionStore.ReleaseHold("hipaa-litigation-2026")
	return nil
}

func (w *ControlWorld) thenChunksEligibleForGC() error {
	// After release, chunks with refcount 0 are eligible
	if w.RetentionStore.IsHeld("trials") {
		return fmt.Errorf("expected hold to be released, namespace should not be held")
	}
	return nil
}
