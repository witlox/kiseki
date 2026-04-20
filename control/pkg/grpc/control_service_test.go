package grpc

import (
	"context"
	"net"
	"testing"

	pb "github.com/witlox/kiseki/control/proto/kiseki/v1"
	"github.com/witlox/kiseki/control/pkg/tenant"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
)

func startTestServer(t *testing.T) (pb.ControlServiceClient, pb.AuditExportServiceClient, func()) {
	t.Helper()

	lis, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}

	store := tenant.NewStore()
	srv := grpc.NewServer()
	pb.RegisterControlServiceServer(srv, NewControlServer(store))
	pb.RegisterAuditExportServiceServer(srv, NewAuditServer())

	go func() { _ = srv.Serve(lis) }()

	conn, err := grpc.NewClient(lis.Addr().String(), grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatalf("dial: %v", err)
	}

	controlClient := pb.NewControlServiceClient(conn)
	auditClient := pb.NewAuditExportServiceClient(conn)

	cleanup := func() {
		conn.Close()
		srv.GracefulStop()
	}

	return controlClient, auditClient, cleanup
}

func TestCreateAndGetOrganization(t *testing.T) {
	client, _, cleanup := startTestServer(t)
	defer cleanup()

	ctx := context.Background()

	// Create org.
	createResp, err := client.CreateOrganization(ctx, &pb.CreateOrganizationRequest{
		Name:           "org-pharma",
		ComplianceTags: []pb.ComplianceTag{pb.ComplianceTag_COMPLIANCE_TAG_HIPAA},
		DedupPolicy:    pb.DedupPolicy_DEDUP_POLICY_CROSS_TENANT,
		Quota: &pb.Quota{
			CapacityBytes:     1_000_000_000,
			Iops:              10000,
			MetadataOpsPerSec: 5000,
		},
	})
	if err != nil {
		t.Fatalf("CreateOrganization: %v", err)
	}

	orgID := createResp.GetOrgId().GetValue()
	if orgID == "" {
		t.Fatal("expected non-empty org ID")
	}

	// Get org back.
	org, err := client.GetOrganization(ctx, &pb.GetOrganizationRequest{
		OrgId: &pb.OrgId{Value: orgID},
	})
	if err != nil {
		t.Fatalf("GetOrganization: %v", err)
	}

	if org.GetName() != "org-pharma" {
		t.Errorf("name = %q, want %q", org.GetName(), "org-pharma")
	}
	if len(org.GetComplianceTags()) != 1 || org.GetComplianceTags()[0] != pb.ComplianceTag_COMPLIANCE_TAG_HIPAA {
		t.Errorf("tags = %v, want [HIPAA]", org.GetComplianceTags())
	}
	if org.GetQuota().GetCapacityBytes() != 1_000_000_000 {
		t.Errorf("capacity = %d, want 1000000000", org.GetQuota().GetCapacityBytes())
	}
}

func TestCreateProjectAndWorkload(t *testing.T) {
	client, _, cleanup := startTestServer(t)
	defer cleanup()

	ctx := context.Background()

	// Create org first.
	orgResp, err := client.CreateOrganization(ctx, &pb.CreateOrganizationRequest{
		Name: "org-test",
		Quota: &pb.Quota{
			CapacityBytes:     1_000_000,
			Iops:              1000,
			MetadataOpsPerSec: 500,
		},
	})
	if err != nil {
		t.Fatalf("CreateOrganization: %v", err)
	}
	orgID := orgResp.GetOrgId()

	// Create project.
	projResp, err := client.CreateProject(ctx, &pb.CreateProjectRequest{
		OrgId: orgID,
		Name:  "proj-alpha",
		Quota: &pb.Quota{
			CapacityBytes:     500_000,
			Iops:              500,
			MetadataOpsPerSec: 250,
		},
	})
	if err != nil {
		t.Fatalf("CreateProject: %v", err)
	}
	if projResp.GetProjectId().GetValue() == "" {
		t.Fatal("expected non-empty project ID")
	}

	// Create workload.
	wlResp, err := client.CreateWorkload(ctx, &pb.CreateWorkloadRequest{
		OrgId:     orgID,
		ProjectId: projResp.GetProjectId(),
		Name:      "wl-training",
		Quota: &pb.Quota{
			CapacityBytes:     100_000,
			Iops:              100,
			MetadataOpsPerSec: 50,
		},
	})
	if err != nil {
		t.Fatalf("CreateWorkload: %v", err)
	}
	if wlResp.GetWorkloadId().GetValue() == "" {
		t.Fatal("expected non-empty workload ID")
	}
}

func TestAccessRequestLifecycle(t *testing.T) {
	client, _, cleanup := startTestServer(t)
	defer cleanup()

	ctx := context.Background()

	// Create org.
	orgResp, _ := client.CreateOrganization(ctx, &pb.CreateOrganizationRequest{
		Name:  "org-access-test",
		Quota: &pb.Quota{CapacityBytes: 1000, Iops: 100, MetadataOpsPerSec: 50},
	})

	// Submit access request.
	reqResp, err := client.RequestAccess(ctx, &pb.AccessRequest{
		Requester:     "cluster-admin-1",
		TenantId:      orgResp.GetOrgId(),
		Scope:         &pb.AccessRequest_Org{Org: orgResp.GetOrgId()},
		DurationHours: 24,
		AccessLevel:   pb.AccessLevel_ACCESS_LEVEL_READ_ONLY,
	})
	if err != nil {
		t.Fatalf("RequestAccess: %v", err)
	}
	reqID := reqResp.GetRequestId()
	if reqID == "" {
		t.Fatal("expected non-empty request ID")
	}

	// Approve it.
	_, err = client.ApproveAccess(ctx, &pb.ApproveAccessRequest{RequestId: reqID})
	if err != nil {
		t.Fatalf("ApproveAccess: %v", err)
	}

	// Double-approve should fail.
	_, err = client.ApproveAccess(ctx, &pb.ApproveAccessRequest{RequestId: reqID})
	if err == nil {
		t.Fatal("expected error on double-approve")
	}
}

func TestGetAuditConfig(t *testing.T) {
	_, auditClient, cleanup := startTestServer(t)
	defer cleanup()

	ctx := context.Background()

	cfg, err := auditClient.GetAuditConfig(ctx, &pb.GetAuditConfigRequest{
		TenantId: &pb.OrgId{Value: "test-tenant"},
	})
	if err != nil {
		t.Fatalf("GetAuditConfig: %v", err)
	}
	if cfg.GetTenantId().GetValue() != "test-tenant" {
		t.Errorf("tenant_id = %q, want %q", cfg.GetTenantId().GetValue(), "test-tenant")
	}
}
