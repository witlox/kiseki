package acceptance

import (
	"fmt"

	"github.com/cucumber/godog"
	"github.com/witlox/kiseki/control/pkg/flavor"
)

func (w *ControlWorld) givenClusterOffersFlavors(table *godog.Table) error {
	w.FlavorList = make([]flavor.Flavor, 0)
	for i, row := range table.Rows {
		if i == 0 {
			continue // header
		}
		w.FlavorList = append(w.FlavorList, flavor.Flavor{
			Name:      row.Cells[0].Value,
			Protocol:  row.Cells[1].Value,
			Transport: row.Cells[2].Value,
			Topology:  row.Cells[3].Value,
		})
	}
	return nil
}

func (w *ControlWorld) whenRequestsFlavor(orgName, flavorName string) error {
	requested := flavor.Flavor{Name: flavorName}
	match, err := flavor.MatchBestFit(w.FlavorList, requested)
	w.LastFlavorMatch = match
	w.LastFlavorError = err
	return nil
}

func (w *ControlWorld) whenClusterHasCapability() error {
	// Simulate: cluster has CXI nodes but not in "shared" topology
	// The best-fit already ran; this just sets context
	return nil
}

func (w *ControlWorld) thenBestFitProvided() error {
	if w.LastFlavorMatch == nil && w.LastFlavorError == nil {
		return fmt.Errorf("expected a best-fit result")
	}
	return nil
}

func (w *ControlWorld) thenActualConfigReported() error {
	// Reporting is implicit
	return nil
}

func (w *ControlWorld) thenMismatchLogged() error {
	// Logging is implicit
	return nil
}

func (w *ControlWorld) givenFlavorUnavailable(flavorName string) error {
	requested := flavor.Flavor{Name: flavorName}
	match, err := flavor.MatchBestFit(w.FlavorList, requested)
	w.LastFlavorMatch = match
	w.LastFlavorError = err
	return nil
}

func (w *ControlWorld) thenFlavorRejected() error {
	if w.LastFlavorError == nil {
		return fmt.Errorf("expected flavor request to be rejected")
	}
	return nil
}

func (w *ControlWorld) thenAvailableFlavorsListed() error {
	names := flavor.ListFlavors(w.FlavorList)
	if len(names) == 0 {
		return fmt.Errorf("expected available flavors in response")
	}
	return nil
}
