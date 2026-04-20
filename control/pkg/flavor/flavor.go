// Package flavor provides placement flavor matching for the control plane.
//
// A Flavor defines a protocol + transport + topology combination that a tenant
// can request. The system performs best-fit matching against available cluster
// capabilities.
//
// Spec: ubiquitous-language.md#Flavor, I-P1.
package flavor

import "fmt"

// Flavor defines a placement capability.
type Flavor struct {
	Name      string
	Protocol  string
	Transport string
	Topology  string
}

// DefaultFlavors returns the standard set of cluster flavors.
func DefaultFlavors() []Flavor {
	return []Flavor{
		{Name: "hpc-slingshot", Protocol: "NFS", Transport: "CXI", Topology: "hyperconverged"},
		{Name: "standard-tcp", Protocol: "S3", Transport: "TCP", Topology: "dedicated"},
		{Name: "ai-training", Protocol: "NFS+S3", Transport: "CXI+TCP", Topology: "shared"},
	}
}

// MatchBestFit finds the best matching flavor from available options.
// Returns the exact match if found, otherwise the closest fit by transport.
func MatchBestFit(available []Flavor, requested Flavor) (*Flavor, error) {
	if len(available) == 0 {
		return nil, fmt.Errorf("no matching flavor available")
	}

	// Exact match
	for i := range available {
		if available[i].Name == requested.Name {
			return &available[i], nil
		}
	}

	// Best-fit: match by transport preference
	for i := range available {
		if available[i].Transport == requested.Transport {
			return &available[i], nil
		}
	}

	// No match at all
	return nil, fmt.Errorf("no matching flavor available")
}

// ListFlavors returns the names of all available flavors.
func ListFlavors(available []Flavor) []string {
	names := make([]string, len(available))
	for i, f := range available {
		names[i] = f.Name
	}
	return names
}
