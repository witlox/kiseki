// Kiseki control-plane API server entry point.
//
// Phase 0 scaffold: compiles cleanly, runs, logs version, and exits.
// Real gRPC surface lands in Phase 11 per specs/architecture/build-phases.md.
package main

import (
	"fmt"
	"os"

	"github.com/witlox/kiseki/control/pkg/version"
)

func main() {
	if _, err := fmt.Fprintf(os.Stdout,
		"kiseki-control %s (commit %s, built %s)\n",
		version.Version, version.Commit, version.BuildTime,
	); err != nil {
		os.Exit(1)
	}
	if _, err := fmt.Fprintln(os.Stderr, "Phase 11 (control plane) not yet implemented."); err != nil {
		os.Exit(1)
	}
	// Exit 0: Phase 0 expectation is a clean scaffold, not a running server.
}
