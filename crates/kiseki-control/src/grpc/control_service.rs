//! gRPC `ControlService` implementation.
//!
//! Wraps in-memory control-plane stores. Maps domain errors to `tonic::Status`.

use std::sync::Arc;

use kiseki_common::tenancy::{ComplianceTag, DedupPolicy, Quota};
use kiseki_proto::v1::control_service_server::ControlService;
use kiseki_proto::v1::{self as pb};
use tonic::{Request, Response, Status};

use crate::error::ControlError;
use crate::federation::{FederationRegistry, Peer};
use crate::flavor;
use crate::iam;
use crate::maintenance::MaintenanceState;
use crate::namespace::{Namespace, NamespaceStore};
use crate::retention::RetentionStore;
use crate::tenant::{Organization, Project, TenantStore, Workload};

/// gRPC handler wrapping the control-plane stores.
pub struct ControlGrpc {
    tenants: Arc<TenantStore>,
    namespaces: NamespaceStore,
    retention: RetentionStore,
    federation: FederationRegistry,
    maintenance: MaintenanceState,
    access_requests: std::sync::Mutex<std::collections::HashMap<String, iam::AccessRequest>>,
}

impl ControlGrpc {
    /// Create a new gRPC handler.
    #[must_use]
    pub fn new(tenants: Arc<TenantStore>) -> Self {
        Self {
            tenants,
            namespaces: NamespaceStore::new(),
            retention: RetentionStore::new(),
            federation: FederationRegistry::new(),
            maintenance: MaintenanceState::new(),
            access_requests: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
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
        request: Request<pb::CreateNamespaceRequest>,
    ) -> Result<Response<pb::CreateNamespaceResponse>, Status> {
        let req = request.into_inner();
        let org_id = req
            .org_id
            .as_ref()
            .map(|o| o.value.clone())
            .unwrap_or_default();
        let id = uuid::Uuid::new_v4().to_string();
        let ns = Namespace {
            id: id.clone(),
            org_id,
            project_id: String::new(),
            shard_id: String::new(),
            compliance_tags: vec![],
            read_only: false,
        };
        self.namespaces.create(ns).map_err(|e| to_status(&e))?;
        // Retrieve to get auto-assigned shard_id.
        let created = self.namespaces.get(&id).map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::CreateNamespaceResponse {
            namespace_id: Some(pb::NamespaceId { value: id }),
            shard_id: Some(pb::ShardId {
                value: created.shard_id,
            }),
        }))
    }

    async fn request_access(
        &self,
        request: Request<pb::AccessRequest>,
    ) -> Result<Response<pb::AccessRequestResponse>, Status> {
        let req = request.into_inner();
        let id = uuid::Uuid::new_v4().to_string();
        let tenant_id = req
            .tenant_id
            .as_ref()
            .map(|o| o.value.clone())
            .unwrap_or_default();
        let (scope, scope_target) = match req.scope {
            Some(pb::access_request::Scope::Namespace(ns)) => {
                (iam::AccessScope::Namespace, ns.value)
            }
            Some(pb::access_request::Scope::Org(org)) => (iam::AccessScope::Tenant, org.value),
            None => (iam::AccessScope::Tenant, String::new()),
        };
        let access_req = iam::AccessRequest::new(
            &id,
            &req.requester,
            &tenant_id,
            scope,
            &scope_target,
            iam::AccessLevel::ReadWrite,
            24,
        );
        self.access_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id.clone(), access_req);
        Ok(Response::new(pb::AccessRequestResponse { request_id: id }))
    }

    async fn approve_access(
        &self,
        request: Request<pb::ApproveAccessRequest>,
    ) -> Result<Response<pb::ApproveAccessResponse>, Status> {
        let req = request.into_inner();
        let mut requests = self
            .access_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ar = requests
            .get_mut(&req.request_id)
            .ok_or_else(|| Status::not_found("access request not found"))?;
        ar.approve().map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::ApproveAccessResponse {}))
    }

    async fn deny_access(
        &self,
        request: Request<pb::DenyAccessRequest>,
    ) -> Result<Response<pb::DenyAccessResponse>, Status> {
        let req = request.into_inner();
        let mut requests = self
            .access_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ar = requests
            .get_mut(&req.request_id)
            .ok_or_else(|| Status::not_found("access request not found"))?;
        ar.deny().map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::DenyAccessResponse {}))
    }

    async fn set_quota(
        &self,
        request: Request<pb::SetQuotaRequest>,
    ) -> Result<Response<pb::SetQuotaResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        // Verify the target scope exists.
        match req.scope {
            Some(pb::set_quota_request::Scope::OrgId(ref org)) => {
                self.tenants
                    .get_org(&org.value)
                    .map_err(|e| to_status(&e))?;
            }
            Some(pb::set_quota_request::Scope::WorkloadId(ref wl)) => {
                self.tenants
                    .get_workload(&wl.value)
                    .map_err(|e| to_status(&e))?;
            }
            None => return Err(Status::invalid_argument("scope required")),
        }
        let _quota = proto_quota(req.quota.as_ref());
        Ok(Response::new(pb::SetQuotaResponse {}))
    }

    async fn set_compliance_tags(
        &self,
        request: Request<pb::SetComplianceTagsRequest>,
    ) -> Result<Response<pb::SetComplianceTagsResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        // Verify scope exists.
        match req.scope {
            Some(pb::set_compliance_tags_request::Scope::OrgId(ref org)) => {
                self.tenants
                    .get_org(&org.value)
                    .map_err(|e| to_status(&e))?;
            }
            Some(pb::set_compliance_tags_request::Scope::NamespaceId(ref ns)) => {
                self.namespaces.get(&ns.value).map_err(|e| to_status(&e))?;
            }
            None => return Err(Status::invalid_argument("scope required")),
        }
        Ok(Response::new(pb::SetComplianceTagsResponse {}))
    }

    async fn set_retention_hold(
        &self,
        request: Request<pb::SetRetentionHoldRequest>,
    ) -> Result<Response<pb::SetRetentionHoldResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        let ns_id = match req.scope {
            Some(pb::set_retention_hold_request::Scope::NamespaceId(ref ns)) => ns.value.clone(),
            Some(pb::set_retention_hold_request::Scope::OrgId(ref org)) => org.value.clone(),
            None => return Err(Status::invalid_argument("scope required")),
        };
        self.retention
            .set_hold(&req.hold_id, &ns_id)
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::SetRetentionHoldResponse {}))
    }

    async fn release_retention_hold(
        &self,
        request: Request<pb::ReleaseRetentionHoldRequest>,
    ) -> Result<Response<pb::ReleaseRetentionHoldResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        self.retention
            .release_hold(&req.hold_id)
            .map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::ReleaseRetentionHoldResponse {}))
    }

    async fn list_flavors(
        &self,
        _request: Request<pb::ListFlavorsRequest>,
    ) -> Result<Response<pb::ListFlavorsResponse>, Status> {
        super::authz::require_admin(&_request)?;
        let flavors = flavor::default_flavors();
        let proto_flavors: Vec<pb::Flavor> = flavors
            .iter()
            .map(|f| pb::Flavor {
                flavor_id: f.name.clone(),
                name: f.name.clone(),
                protocols: vec![f.protocol.clone()],
                transports: vec![f.transport.clone()],
                topology: f.topology.clone(),
            })
            .collect();
        Ok(Response::new(pb::ListFlavorsResponse {
            flavors: proto_flavors,
        }))
    }

    async fn match_flavor(
        &self,
        request: Request<pb::MatchFlavorRequest>,
    ) -> Result<Response<pb::MatchFlavorResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        let available = flavor::default_flavors();
        let requested_proto = req.requested.unwrap_or_default();
        let requested = flavor::Flavor {
            name: requested_proto.name,
            protocol: requested_proto
                .protocols
                .first()
                .cloned()
                .unwrap_or_default(),
            transport: requested_proto
                .transports
                .first()
                .cloned()
                .unwrap_or_default(),
            topology: requested_proto.topology,
        };
        let matched = flavor::match_best_fit(&available, &requested);
        let provided = matched.map(|f| pb::Flavor {
            flavor_id: f.name.clone(),
            name: f.name,
            protocols: vec![f.protocol],
            transports: vec![f.transport],
            topology: f.topology,
        });
        Ok(Response::new(pb::MatchFlavorResponse {
            provided,
            mismatches: vec![],
        }))
    }

    async fn register_peer(
        &self,
        request: Request<pb::RegisterPeerRequest>,
    ) -> Result<Response<pb::RegisterPeerResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        let peer_id = uuid::Uuid::new_v4().to_string();
        let peer = Peer {
            site_id: req.site_name.clone(),
            endpoint: req.endpoint,
            connected: false,
            replication_mode: "async".to_owned(),
            config_sync: false,
            data_cipher_only: false,
        };
        self.federation.register(peer).map_err(|e| to_status(&e))?;
        Ok(Response::new(pb::RegisterPeerResponse { peer_id }))
    }

    async fn list_peers(
        &self,
        _request: Request<pb::ListPeersRequest>,
    ) -> Result<Response<pb::ListPeersResponse>, Status> {
        super::authz::require_admin(&_request)?;
        let peers = self.federation.list_peers();
        let proto_peers = peers
            .into_iter()
            .map(|p| pb::FederationPeer {
                peer_id: p.site_id.clone(),
                site_name: p.site_id,
                endpoint: p.endpoint,
                status: if p.connected {
                    "connected".to_owned()
                } else {
                    "disconnected".to_owned()
                },
            })
            .collect();
        Ok(Response::new(pb::ListPeersResponse { peers: proto_peers }))
    }

    async fn set_maintenance_mode(
        &self,
        request: Request<pb::SetMaintenanceModeRequest>,
    ) -> Result<Response<pb::SetMaintenanceModeResponse>, Status> {
        super::authz::require_admin(&request)?;
        let req = request.into_inner();
        if req.enabled {
            self.maintenance.enable();
            self.namespaces.set_read_only(true);
        } else {
            self.maintenance.disable();
            self.namespaces.set_read_only(false);
        }
        Ok(Response::new(pb::SetMaintenanceModeResponse {}))
    }
}
