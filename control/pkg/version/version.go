// Package version holds the Kiseki control-plane binary version metadata.
//
// Populated at link time via -ldflags in CI. Defaults are suitable for
// local development builds.
package version

// Version is the semantic version of the control-plane binary.
var Version = "0.0.0-dev"

// Commit is the git commit hash this binary was built from.
var Commit = "unknown"

// BuildTime is the UTC timestamp of the build.
var BuildTime = "unknown"
