package grpc

import (
	"context"

	pb "github.com/witlox/kiseki/control/proto/kiseki/v1"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// AuditServer implements the AuditExportService gRPC server.
type AuditServer struct {
	pb.UnimplementedAuditExportServiceServer
}

// NewAuditServer creates a new AuditExportService server.
func NewAuditServer() *AuditServer {
	return &AuditServer{}
}

// StreamTenantAudit streams audit events for a tenant.
func (s *AuditServer) StreamTenantAudit(_ *pb.StreamTenantAuditRequest, _ pb.AuditExportService_StreamTenantAuditServer) error {
	// TODO: Wire to audit store. Returns empty stream for now.
	return nil
}

// GetAuditConfig returns the audit export configuration.
func (s *AuditServer) GetAuditConfig(_ context.Context, req *pb.GetAuditConfigRequest) (*pb.AuditExportConfig, error) {
	if req.GetTenantId() == nil {
		return nil, status.Errorf(codes.InvalidArgument, "tenant_id required")
	}
	return &pb.AuditExportConfig{
		TenantId: req.GetTenantId(),
	}, nil
}

// SetAuditConfig updates the audit export configuration.
func (s *AuditServer) SetAuditConfig(_ context.Context, _ *pb.SetAuditConfigRequest) (*pb.SetAuditConfigResponse, error) {
	return &pb.SetAuditConfigResponse{}, nil
}

// Ensure compile-time interface compliance.
var _ pb.AuditExportServiceServer = (*AuditServer)(nil)
