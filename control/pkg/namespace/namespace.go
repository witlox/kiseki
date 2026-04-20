// Package namespace provides namespace management for the control plane.
//
// A Namespace is a logical container within a tenant (org/project) that
// maps to one or more shards. Compliance tags inherit downward from the
// org/project hierarchy.
//
// Spec: ubiquitous-language.md#Namespace, I-T1.
package namespace

import (
	"fmt"
	"sync"

	"github.com/witlox/kiseki/control/pkg/tenant"
)

// Namespace represents a storage namespace within a tenant hierarchy.
type Namespace struct {
	ID             string
	OrgID          string
	ProjectID      string
	ShardID        string
	ComplianceTags []tenant.ComplianceTag
	ReadOnly       bool
}

// Store provides namespace CRUD operations.
type Store struct {
	mu         sync.RWMutex
	namespaces map[string]*Namespace
	shardSeq   int
}

// NewStore creates an empty namespace store.
func NewStore() *Store {
	return &Store{
		namespaces: make(map[string]*Namespace),
	}
}

// Create creates a new namespace, assigning a shard automatically.
func (s *Store) Create(ns *Namespace) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.namespaces[ns.ID]; exists {
		return fmt.Errorf("namespace %s already exists", ns.ID)
	}
	if ns.ShardID == "" {
		s.shardSeq++
		ns.ShardID = fmt.Sprintf("shard-%04d", s.shardSeq)
	}
	s.namespaces[ns.ID] = ns
	return nil
}

// Get retrieves a namespace by ID.
func (s *Store) Get(id string) (*Namespace, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	ns, ok := s.namespaces[id]
	if !ok {
		return nil, fmt.Errorf("namespace %s not found", id)
	}
	return ns, nil
}

// SetReadOnly sets the read-only flag on all namespaces.
func (s *Store) SetReadOnly(readOnly bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, ns := range s.namespaces {
		ns.ReadOnly = readOnly
	}
}

// List returns all namespaces.
func (s *Store) List() []*Namespace {
	s.mu.RLock()
	defer s.mu.RUnlock()
	result := make([]*Namespace, 0, len(s.namespaces))
	for _, ns := range s.namespaces {
		result = append(result, ns)
	}
	return result
}
