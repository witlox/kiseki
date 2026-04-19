package tenant

import (
	"fmt"
	"sync"
)

// Store provides tenant CRUD operations.
type Store struct {
	mu        sync.RWMutex
	orgs      map[string]*Organization
	projects  map[string]*Project  // keyed by project ID
	workloads map[string]*Workload // keyed by workload ID
}

// NewStore creates an empty tenant store.
func NewStore() *Store {
	return &Store{
		orgs:      make(map[string]*Organization),
		projects:  make(map[string]*Project),
		workloads: make(map[string]*Workload),
	}
}

// CreateOrg creates a new organization.
func (s *Store) CreateOrg(org *Organization) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, exists := s.orgs[org.ID]; exists {
		return fmt.Errorf("organization %s already exists", org.ID)
	}
	s.orgs[org.ID] = org
	return nil
}

// GetOrg retrieves an organization by ID.
func (s *Store) GetOrg(id string) (*Organization, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	org, ok := s.orgs[id]
	if !ok {
		return nil, fmt.Errorf("organization %s not found", id)
	}
	return org, nil
}

// DeleteOrg removes an organization.
func (s *Store) DeleteOrg(id string) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	if _, ok := s.orgs[id]; !ok {
		return fmt.Errorf("organization %s not found", id)
	}
	delete(s.orgs, id)
	return nil
}

// CreateProject creates a project within an organization.
func (s *Store) CreateProject(proj *Project) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	org, ok := s.orgs[proj.OrgID]
	if !ok {
		return fmt.Errorf("organization %s not found", proj.OrgID)
	}
	if err := ValidateQuota(org.Quota, proj.Quota); err != nil {
		return fmt.Errorf("project quota: %w", err)
	}
	s.projects[proj.ID] = proj
	return nil
}

// GetProject retrieves a project by ID.
func (s *Store) GetProject(id string) (*Project, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	proj, ok := s.projects[id]
	if !ok {
		return nil, fmt.Errorf("project %s not found", id)
	}
	return proj, nil
}

// CreateWorkload creates a workload within a tenant.
func (s *Store) CreateWorkload(wl *Workload) error {
	s.mu.Lock()
	defer s.mu.Unlock()
	org, ok := s.orgs[wl.OrgID]
	if !ok {
		return fmt.Errorf("organization %s not found", wl.OrgID)
	}
	if err := ValidateQuota(org.Quota, wl.Quota); err != nil {
		return fmt.Errorf("workload quota: %w", err)
	}
	s.workloads[wl.ID] = wl
	return nil
}

// GetWorkload retrieves a workload by ID.
func (s *Store) GetWorkload(id string) (*Workload, error) {
	s.mu.RLock()
	defer s.mu.RUnlock()
	wl, ok := s.workloads[id]
	if !ok {
		return nil, fmt.Errorf("workload %s not found", id)
	}
	return wl, nil
}

// OrgCount returns the number of organizations.
func (s *Store) OrgCount() int {
	s.mu.RLock()
	defer s.mu.RUnlock()
	return len(s.orgs)
}
