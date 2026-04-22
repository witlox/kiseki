//! gRPC `ControlService` implementation.
//!
//! Wraps in-memory tenant store. Maps domain errors to `tonic::Status`.

use std::sync::Arc;

use kiseki_common::tenancy::{ComplianceTag, DedupPolicy, Quota};
use kiseki_proto::v1::control_service_server::ControlService;
use kiseki_proto::v1::{self as pb};
use tonic::{Request, Response, Status};

use crate::error::ControlError;
use crate::tenant::{Organization, Project, TenantStore, Workload};

/// gRPC handler wrapping the control-plane stores.
pub struct ControlGrpc {
    tenants: Arc<TenantStore>,
}

impl ControlGrpc {
    /// Create a new gRPC handler.
    #[must_use]
    pub fn new(tenants: Arc<TenantStore>) -> Self {
        Self { tenants }
    }
}

fn to_status(e: &ControlError) -> Status {
    match e {
        ControlError::AlreadyExists(msg) => Status::already_exists(msg.clone()),
        ControlError::NotFound(msg) => Status::not_found(msg.clone()),
        ControlError::QuotaExceeded(msg) | ControlError::Rejected(msg) => {
            Status::failed_precondition(msg.clone())
        }
    }
}

fn proto_tags(tags: &[i32]) -> Vec<ComplianceTag> {
    tags.iter()
        .filter_map(|&t| match t {
            1 => Some(ComplianceTag::Hipaa),
            2 => Some(ComplianceTag::Gdpr),
            3 => Some(ComplianceTag::RevFadp),
            4 => Some(ComplianceTag::SwissResidency),
            _ => None,
        })
        .collect()
}

fn proto_quota(q: Option<&pb::Quota>) -> Quota {
    q.map_or(
        Quota {
            capacity_bytes: 0,
            iops: 0,
            metadata_ops_per_sec: 0,
        },
        |q| Quota {
            capacity_bytes: q.capacity_bytes,
            iops: q.iops,
            metadata_ops_per_sec: q.metadata_ops_per_sec,
        },
    )
}

#[tonic::async_trait]
impl ControlService for ControlGrpc {
    async fn create_organization(
        &self,
        request: Request<pb::CreateOrganizationRequest>,
    ) -> Result<Response<pb::CreateOrganizationResponse>, Status> {
        let req = request.into_inner();
        let id = uuid::Uuid::new_v4().to_string();
        let org = Organization {
            id: id.clone(),
            name: req.name,
            compliance_tags: proto_tags(&req.compliance_tags),
            dedup_policy: DedupPolicy::CrossTenant,
            quota: proto_quota(req.quota.as_ref()),
        };
        self.tenants.create_org(org).map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::CreateOrganizationResponse {
            org_id: Some(pb::OrgId { value: id }),
        }))
    }

    async fn create_project(
        &self,
        request: Request<pb::CreateProjectRequest>,
    ) -> Result<Response<pb::CreateProjectResponse>, Status> {
        let req = request.into_inner();
        let org_id = req
            .org_id
            .as_ref()
            .map(|o| o.value.clone())
            .unwrap_or_default();
        let id = uuid::Uuid::new_v4().to_string();
        let proj = Project {
            id: id.clone(),
            org_id,
            name: req.name,
            compliance_tags: proto_tags(&req.compliance_tags),
            quota: proto_quota(req.quota.as_ref()),
        };
        self.tenants
            .create_project(proj)
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::CreateProjectResponse {
            project_id: Some(pb::ProjectId { value: id }),
        }))
    }

    async fn create_workload(
        &self,
        request: Request<pb::CreateWorkloadRequest>,
    ) -> Result<Response<pb::CreateWorkloadResponse>, Status> {
        let req = request.into_inner();
        let org_id = req
            .org_id
            .as_ref()
            .map(|o| o.value.clone())
            .unwrap_or_default();
        let project_id = req
            .project_id
            .as_ref()
            .map(|p| p.value.clone())
            .unwrap_or_default();
        let id = uuid::Uuid::new_v4().to_string();
        let wl = Workload {
            id: id.clone(),
            org_id,
            project_id,
            name: req.name,
            quota: proto_quota(req.quota.as_ref()),
        };
        self.tenants
            .create_workload(wl)
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::CreateWorkloadResponse {
            workload_id: Some(pb::WorkloadId { value: id }),
        }))
    }

    async fn get_organization(
        &self,
        request: Request<pb::GetOrganizationRequest>,
    ) -> Result<Response<pb::Organization>, Status> {
        let req = request.into_inner();
        let org_id = req.org_id.as_ref().map_or("", |o| o.value.as_str());
        let org = self.tenants.get_org(org_id).map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::Organization {
            org_id: Some(pb::OrgId { value: org.id }),
            name: org.name,
            compliance_tags: vec![],
            quota: None,
            usage: None,
            created_at: None,
            dedup_policy: 0,
        }))
    }

    async fn list_organizations(
        &self,
        _request: Request<pb::ListOrganizationsRequest>,
    ) -> Result<Response<pb::ListOrganizationsResponse>, Status> {
        let orgs = self.tenants.list_orgs();
        let proto_orgs: Vec<pb::Organization> = orgs
            .into_iter()
            .map(|o| pb::Organization {
                org_id: Some(pb::OrgId { value: o.id }),
                name: o.name,
                compliance_tags: vec![],
                dedup_policy: 0,
                quota: None,
                usage: None,
                created_at: None,
            })
            .collect();
        Ok(Response::new(pb::ListOrganizationsResponse {
            organizations: proto_orgs,
        }))
    }

    async fn create_namespace(
        &self,
        _request: Request<pb::CreateNamespaceRequest>,
    ) -> Result<Response<pb::CreateNamespaceResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn request_access(
        &self,
        _request: Request<pb::AccessRequest>,
    ) -> Result<Response<pb::AccessRequestResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn approve_access(
        &self,
        _request: Request<pb::ApproveAccessRequest>,
    ) -> Result<Response<pb::ApproveAccessResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn deny_access(
        &self,
        _request: Request<pb::DenyAccessRequest>,
    ) -> Result<Response<pb::DenyAccessResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn set_quota(
        &self,
        _request: Request<pb::SetQuotaRequest>,
    ) -> Result<Response<pb::SetQuotaResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn set_compliance_tags(
        &self,
        _request: Request<pb::SetComplianceTagsRequest>,
    ) -> Result<Response<pb::SetComplianceTagsResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn set_retention_hold(
        &self,
        _request: Request<pb::SetRetentionHoldRequest>,
    ) -> Result<Response<pb::SetRetentionHoldResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn release_retention_hold(
        &self,
        _request: Request<pb::ReleaseRetentionHoldRequest>,
    ) -> Result<Response<pb::ReleaseRetentionHoldResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn list_flavors(
        &self,
        _request: Request<pb::ListFlavorsRequest>,
    ) -> Result<Response<pb::ListFlavorsResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn match_flavor(
        &self,
        _request: Request<pb::MatchFlavorRequest>,
    ) -> Result<Response<pb::MatchFlavorResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn register_peer(
        &self,
        _request: Request<pb::RegisterPeerRequest>,
    ) -> Result<Response<pb::RegisterPeerResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn list_peers(
        &self,
        _request: Request<pb::ListPeersRequest>,
    ) -> Result<Response<pb::ListPeersResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }

    async fn set_maintenance_mode(
        &self,
        _request: Request<pb::SetMaintenanceModeRequest>,
    ) -> Result<Response<pb::SetMaintenanceModeResponse>, Status> {
        Err(Status::unimplemented("not yet wired"))
    }
}
