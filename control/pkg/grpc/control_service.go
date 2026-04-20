// Package grpc implements the ControlService and AuditExportService gRPC servers.
package grpc

import (
	"context"
	"sync"

	"github.com/gofrs/uuid"
	pb "github.com/witlox/kiseki/control/proto/kiseki/v1"
	"github.com/witlox/kiseki/control/pkg/iam"
	"github.com/witlox/kiseki/control/pkg/tenant"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

// ControlServer implements the ControlService gRPC server.
type ControlServer struct {
	pb.UnimplementedControlServiceServer

	tenants    *tenant.Store
	accessReqs map[string]*iam.AccessRequest
	mu         sync.RWMutex
}

// NewControlServer creates a new ControlService backed by the given tenant store.
func NewControlServer(tenants *tenant.Store) *ControlServer {
	return &ControlServer{
		tenants:    tenants,
		accessReqs: make(map[string]*iam.AccessRequest),
	}
}

func protoComplianceTags(tags []pb.ComplianceTag) []tenant.ComplianceTag {
	var result []tenant.ComplianceTag
	for _, t := range tags {
		switch t {
		case pb.ComplianceTag_COMPLIANCE_TAG_HIPAA:
			result = append(result, tenant.TagHIPAA)
		case pb.ComplianceTag_COMPLIANCE_TAG_GDPR:
			result = append(result, tenant.TagGDPR)
		case pb.ComplianceTag_COMPLIANCE_TAG_REVFADP:
			result = append(result, tenant.TagRevFADP)
		case pb.ComplianceTag_COMPLIANCE_TAG_SWISS_RESIDENCY:
			result = append(result, tenant.TagSwissResidency)
		default:
			// skip unspecified
		}
	}
	return result
}

func domainTagsToProto(tags []tenant.ComplianceTag) []pb.ComplianceTag {
	var result []pb.ComplianceTag
	for _, t := range tags {
		switch t {
		case tenant.TagHIPAA:
			result = append(result, pb.ComplianceTag_COMPLIANCE_TAG_HIPAA)
		case tenant.TagGDPR:
			result = append(result, pb.ComplianceTag_COMPLIANCE_TAG_GDPR)
		case tenant.TagRevFADP:
			result = append(result, pb.ComplianceTag_COMPLIANCE_TAG_REVFADP)
		case tenant.TagSwissResidency:
			result = append(result, pb.ComplianceTag_COMPLIANCE_TAG_SWISS_RESIDENCY)
		}
	}
	return result
}

func protoQuota(q *pb.Quota) tenant.Quota {
	if q == nil {
		return tenant.Quota{}
	}
	return tenant.Quota{
		CapacityBytes:     q.GetCapacityBytes(),
		IOPS:              q.GetIops(),
		MetadataOpsPerSec: q.GetMetadataOpsPerSec(),
	}
}

func protoDedupPolicy(p pb.DedupPolicy) tenant.DedupPolicy {
	if p == pb.DedupPolicy_DEDUP_POLICY_TENANT_ISOLATED {
		return tenant.DedupTenantIsolated
	}
	return tenant.DedupCrossTenant
}

// CreateOrganization creates a new tenant organization.
func (s *ControlServer) CreateOrganization(_ context.Context, req *pb.CreateOrganizationRequest) (*pb.CreateOrganizationResponse, error) {
	if req.GetName() == "" {
		return nil, status.Errorf(codes.InvalidArgument, "name is required")
	}

	id, err := uuid.NewV4()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "generate org ID: %v", err)
	}

	org := &tenant.Organization{
		ID:             id.String(),
		Name:           req.GetName(),
		ComplianceTags: protoComplianceTags(req.GetComplianceTags()),
		DedupPolicy:    protoDedupPolicy(req.GetDedupPolicy()),
		Quota:          protoQuota(req.GetQuota()),
	}

	if err := s.tenants.CreateOrg(org); err != nil {
		return nil, status.Errorf(codes.AlreadyExists, "%v", err)
	}

	return &pb.CreateOrganizationResponse{
		OrgId: &pb.OrgId{Value: org.ID},
	}, nil
}

// CreateProject creates a project within an organization.
func (s *ControlServer) CreateProject(_ context.Context, req *pb.CreateProjectRequest) (*pb.CreateProjectResponse, error) {
	if req.GetName() == "" {
		return nil, status.Errorf(codes.InvalidArgument, "name is required")
	}
	if req.GetOrgId() == nil {
		return nil, status.Errorf(codes.InvalidArgument, "org_id is required")
	}

	id, err := uuid.NewV4()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "generate project ID: %v", err)
	}

	proj := &tenant.Project{
		ID:             id.String(),
		OrgID:          req.GetOrgId().GetValue(),
		Name:           req.GetName(),
		ComplianceTags: protoComplianceTags(req.GetComplianceTags()),
		Quota:          protoQuota(req.GetQuota()),
	}

	if err := s.tenants.CreateProject(proj); err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}

	return &pb.CreateProjectResponse{
		ProjectId: &pb.ProjectId{Value: proj.ID},
	}, nil
}

// CreateWorkload creates a workload within an organization.
func (s *ControlServer) CreateWorkload(_ context.Context, req *pb.CreateWorkloadRequest) (*pb.CreateWorkloadResponse, error) {
	if req.GetName() == "" {
		return nil, status.Errorf(codes.InvalidArgument, "name is required")
	}
	if req.GetOrgId() == nil {
		return nil, status.Errorf(codes.InvalidArgument, "org_id is required")
	}

	id, err := uuid.NewV4()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "generate workload ID: %v", err)
	}

	projID := ""
	if req.GetProjectId() != nil {
		projID = req.GetProjectId().GetValue()
	}

	wl := &tenant.Workload{
		ID:     id.String(),
		OrgID:  req.GetOrgId().GetValue(),
		ProjID: projID,
		Name:   req.GetName(),
		Quota:  protoQuota(req.GetQuota()),
	}

	if err := s.tenants.CreateWorkload(wl); err != nil {
		return nil, status.Errorf(codes.InvalidArgument, "%v", err)
	}

	return &pb.CreateWorkloadResponse{
		WorkloadId: &pb.WorkloadId{Value: wl.ID},
	}, nil
}

// GetOrganization retrieves an organization by ID.
func (s *ControlServer) GetOrganization(_ context.Context, req *pb.GetOrganizationRequest) (*pb.Organization, error) {
	org, err := s.tenants.GetOrg(req.GetOrgId().GetValue())
	if err != nil {
		return nil, status.Errorf(codes.NotFound, "%v", err)
	}

	dedupPolicy := pb.DedupPolicy_DEDUP_POLICY_CROSS_TENANT
	if org.DedupPolicy == tenant.DedupTenantIsolated {
		dedupPolicy = pb.DedupPolicy_DEDUP_POLICY_TENANT_ISOLATED
	}

	return &pb.Organization{
		OrgId:          &pb.OrgId{Value: org.ID},
		Name:           org.Name,
		ComplianceTags: domainTagsToProto(org.ComplianceTags),
		DedupPolicy:    dedupPolicy,
		Quota: &pb.Quota{
			CapacityBytes:     org.Quota.CapacityBytes,
			Iops:              org.Quota.IOPS,
			MetadataOpsPerSec: org.Quota.MetadataOpsPerSec,
		},
	}, nil
}

// ListOrganizations returns all organizations.
func (s *ControlServer) ListOrganizations(_ context.Context, _ *pb.ListOrganizationsRequest) (*pb.ListOrganizationsResponse, error) {
	orgs := s.tenants.ListOrgs()
	var protoOrgs []*pb.Organization
	for _, org := range orgs {
		dedupPolicy := pb.DedupPolicy_DEDUP_POLICY_CROSS_TENANT
		if org.DedupPolicy == tenant.DedupTenantIsolated {
			dedupPolicy = pb.DedupPolicy_DEDUP_POLICY_TENANT_ISOLATED
		}
		protoOrgs = append(protoOrgs, &pb.Organization{
			OrgId:          &pb.OrgId{Value: org.ID},
			Name:           org.Name,
			ComplianceTags: domainTagsToProto(org.ComplianceTags),
			DedupPolicy:    dedupPolicy,
			Quota: &pb.Quota{
				CapacityBytes:     org.Quota.CapacityBytes,
				Iops:              org.Quota.IOPS,
				MetadataOpsPerSec: org.Quota.MetadataOpsPerSec,
			},
		})
	}
	return &pb.ListOrganizationsResponse{Organizations: protoOrgs}, nil
}

// RequestAccess submits an access request (I-T4: deny by default).
func (s *ControlServer) RequestAccess(_ context.Context, req *pb.AccessRequest) (*pb.AccessRequestResponse, error) {
	id, err := uuid.NewV4()
	if err != nil {
		return nil, status.Errorf(codes.Internal, "generate request ID: %v", err)
	}

	scope := iam.ScopeTenant
	if _, ok := req.GetScope().(*pb.AccessRequest_Namespace); ok {
		scope = iam.ScopeNamespace
	}

	level := iam.AccessReadOnly
	if req.GetAccessLevel() == pb.AccessLevel_ACCESS_LEVEL_READ_WRITE {
		level = iam.AccessReadWrite
	}

	scopeTarget := ""
	if ns, ok := req.GetScope().(*pb.AccessRequest_Namespace); ok {
		scopeTarget = ns.Namespace.GetValue()
	}

	accessReq := &iam.AccessRequest{
		ID:            id.String(),
		RequesterID:   req.GetRequester(),
		TenantID:      req.GetTenantId().GetValue(),
		Scope:         scope,
		ScopeTarget:   scopeTarget,
		AccessLevel:   level,
		DurationHours: int(req.GetDurationHours()),
		Status:        iam.StatusPending,
	}

	s.mu.Lock()
	s.accessReqs[accessReq.ID] = accessReq
	s.mu.Unlock()

	return &pb.AccessRequestResponse{
		RequestId: id.String(),
	}, nil
}

// ApproveAccess approves a pending access request.
func (s *ControlServer) ApproveAccess(_ context.Context, req *pb.ApproveAccessRequest) (*pb.ApproveAccessResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	ar, ok := s.accessReqs[req.GetRequestId()]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "access request %s not found", req.GetRequestId())
	}

	if err := ar.Approve(); err != nil {
		return nil, status.Errorf(codes.FailedPrecondition, "%v", err)
	}

	return &pb.ApproveAccessResponse{}, nil
}

// DenyAccess denies a pending access request.
func (s *ControlServer) DenyAccess(_ context.Context, req *pb.DenyAccessRequest) (*pb.DenyAccessResponse, error) {
	s.mu.Lock()
	defer s.mu.Unlock()

	ar, ok := s.accessReqs[req.GetRequestId()]
	if !ok {
		return nil, status.Errorf(codes.NotFound, "access request %s not found", req.GetRequestId())
	}

	if err := ar.Deny(); err != nil {
		return nil, status.Errorf(codes.FailedPrecondition, "%v", err)
	}

	return &pb.DenyAccessResponse{}, nil
}

// SetMaintenanceMode toggles maintenance mode.
func (s *ControlServer) SetMaintenanceMode(_ context.Context, _ *pb.SetMaintenanceModeRequest) (*pb.SetMaintenanceModeResponse, error) {
	return &pb.SetMaintenanceModeResponse{}, nil
}

// Ensure compile-time interface compliance.
var _ pb.ControlServiceServer = (*ControlServer)(nil)
