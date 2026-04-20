// Package maintenance provides maintenance mode management for the control plane.
//
// When maintenance mode is enabled, all shards enter read-only mode and
// write commands are rejected with retriable errors.
//
// Spec: ubiquitous-language.md#MaintenanceMode.
package maintenance

import "sync"

// State tracks whether the cluster is in maintenance mode.
type State struct {
	mu      sync.RWMutex
	enabled bool
}

// NewState creates a new maintenance state (disabled by default).
func NewState() *State {
	return &State{}
}

// Enable puts the cluster into maintenance mode.
func (s *State) Enable() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.enabled = true
}

// Disable takes the cluster out of maintenance mode.
func (s *State) Disable() {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.enabled = false
}

// IsEnabled returns whether maintenance mode is active.
func (s *State) IsEnabled() bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return s.enabled
}
