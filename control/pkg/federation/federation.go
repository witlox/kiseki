// Package federation provides federation peer management for the control plane.
//
// Federation allows multiple Kiseki sites to replicate tenant config and
// discovery metadata asynchronously. Data replication carries ciphertext only.
//
// Spec: ubiquitous-language.md#Federation, I-F1.
package federation

import (
	"fmt"
	"sync"
)

// Peer represents a federated site.
type Peer struct {
	SiteID          string
	Endpoint        string
	Connected       bool
	ReplicationMode string // "async" or "sync"
	ConfigSync      bool
	DataCipherOnly  bool
}

// Registry manages federation peers.
type Registry struct {
	mu    sync.RWMutex
	peers map[string]*Peer
}

// NewRegistry creates an empty federation registry.
func NewRegistry() *Registry {
	return &Registry{
		peers: make(map[string]*Peer),
	}
}

// Register adds or updates a federation peer.
func (r *Registry) Register(peer *Peer) error {
	r.mu.Lock()
	defer r.mu.Unlock()
	if peer.SiteID == "" {
		return fmt.Errorf("site ID required")
	}
	peer.Connected = true
	r.peers[peer.SiteID] = peer
	return nil
}

// ListPeers returns all registered peers.
func (r *Registry) ListPeers() []*Peer {
	r.mu.RLock()
	defer r.mu.RUnlock()
	result := make([]*Peer, 0, len(r.peers))
	for _, p := range r.peers {
		result = append(result, p)
	}
	return result
}

// GetPeer retrieves a peer by site ID.
func (r *Registry) GetPeer(siteID string) (*Peer, error) {
	r.mu.RLock()
	defer r.mu.RUnlock()
	p, ok := r.peers[siteID]
	if !ok {
		return nil, fmt.Errorf("peer %s not found", siteID)
	}
	return p, nil
}

// IsConnected checks if a specific site is connected.
func (r *Registry) IsConnected(siteID string) bool {
	r.mu.RLock()
	defer r.mu.RUnlock()
	p, ok := r.peers[siteID]
	return ok && p.Connected
}
