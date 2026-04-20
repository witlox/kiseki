// Package retention provides retention hold management for the control plane.
//
// Retention holds prevent physical GC of chunks even when refcount drops
// to zero. Used for litigation holds, compliance, etc.
//
// Spec: ubiquitous-language.md#RetentionHold, I-R1.
package retention

import (
	"fmt"
	"sync"
)

// Hold represents a retention hold on a namespace.
type Hold struct {
	Name        string
	NamespaceID string
	Active      bool
}

// Store provides retention hold management.
type Store struct {
	mu    sync.RWMutex
	holds map[string]*Hold
}

// NewStore creates an empty retention store.
func NewStore() *Store {
	return &Store{
		holds: make(map[string]*Hold),
	}
}

// SetHold creates or activates a retention hold.
func (s *Store) SetHold(name, nsID string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if name == "" {
		return fmt.Errorf("hold name required")
	}
	s.holds[name] = &Hold{
		Name:        name,
		NamespaceID: nsID,
		Active:      true,
	}
	return nil
}

// ReleaseHold deactivates a retention hold.
func (s *Store) ReleaseHold(name string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	h, ok := s.holds[name]
	if !ok {
		return fmt.Errorf("hold %s not found", name)
	}
	h.Active = false
	return nil
}

// IsHeld checks if any active hold exists for the given namespace.
func (s *Store) IsHeld(nsID string) bool {
	s.mu.RLock()
	defer s.mu.RUnlock()
	for _, h := range s.holds {
		if h.NamespaceID == nsID && h.Active {
			return true
		}
	}
	return false
}

// GetHold retrieves a hold by name.
func (s *Store) GetHold(name string) (*Hold, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	h, ok := s.holds[name]
	if !ok {
		return nil, fmt.Errorf("hold %s not found", name)
	}
	return h, nil
}
