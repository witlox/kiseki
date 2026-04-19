// Kiseki admin CLI entry point.
//
// Phase 0 scaffold: compiles cleanly, prints version, and exits. Real
// subcommands land alongside Phase 11 per
// specs/architecture/build-phases.md.
package main

import (
	"fmt"
	"os"

	"github.com/witlox/kiseki/control/pkg/version"
)

func main() {
	if _, err := fmt.Fprintf(os.Stdout,
		"kiseki-cli %s (commit %s, built %s)\n",
		version.Version, version.Commit, version.BuildTime,
	); err != nil {
		os.Exit(1)
	}
	if _, err := fmt.Fprintln(os.Stderr, "Phase 11 (control plane CLI) not yet implemented."); err != nil {
		os.Exit(1)
	}
	// Exit 0: Phase 0 expectation is a clean scaffold.
}
