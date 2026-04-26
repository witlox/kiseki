//! gRPC service implementation for the Key Manager.
//!
//! Wraps a `dyn KeyManagerOps` behind the tonic-generated
//! `KeyManagerService` trait. Maps domain errors to `tonic::Status`.

use std::sync::Arc;

use kiseki_common::tenancy::KeyEpoch as DomainKeyEpoch;
use kiseki_proto::v1::key_manager_service_server::KeyManagerService;
use kiseki_proto::v1::{
    CurrentEpochRequest, CurrentEpochResponse, FetchMasterKeyRequest, FetchMasterKeyResponse,
    KeyManagerHealthRequest, KeyManagerHealthResponse, RotateSystemKeyRequest,
    RotateSystemKeyResponse,
};
use tonic::{Request, Response, Status};

use crate::epoch::KeyManagerOps;
use crate::error::KeyManagerError;

/// gRPC handler wrapping a `KeyManagerOps` implementation.
pub struct KeyManagerGrpc<T: KeyManagerOps> {
    ops: Arc<T>,
}

impl<T: KeyManagerOps> KeyManagerGrpc<T> {
    /// Create a new gRPC handler.
    #[must_use]
    pub fn new(ops: Arc<T>) -> Self {
        Self { ops }
    }
}

fn to_status(e: &KeyManagerError) -> Status {
    match e {
        KeyManagerError::EpochNotFound(_) => Status::not_found(e.to_string()),
        KeyManagerError::KeyGenerationFailed => Status::internal(e.to_string()),
        KeyManagerError::Unavailable => Status::unavailable(e.to_string()),
        KeyManagerError::RotationInProgress => Status::already_exists(e.to_string()),
    }
}

/// Convert a domain `KeyEpoch` to the proto `KeyEpoch` message.
fn to_proto_epoch(epoch: DomainKeyEpoch) -> kiseki_proto::v1::KeyEpoch {
    kiseki_proto::v1::KeyEpoch { value: epoch.0 }
}

#[tonic::async_trait]
impl<T: KeyManagerOps + Send + Sync + 'static> KeyManagerService for KeyManagerGrpc<T> {
    async fn fetch_master_key(
        &self,
        request: Request<FetchMasterKeyRequest>,
    ) -> Result<Response<FetchMasterKeyResponse>, Status> {
        let _s = kiseki_tracing::span("KeyManagerService.FetchMasterKey");
        let req = request.into_inner();
        let epoch_val = req
            .epoch
            .ok_or_else(|| Status::invalid_argument("epoch required"))?;
        let epoch = DomainKeyEpoch(epoch_val.value);

        // Verify the epoch exists — key material is NOT sent over gRPC.
        // Storage nodes receive the master key via a secure bootstrap
        // channel, not this RPC.
        let _key = self
            .ops
            .fetch_master_key(epoch)
            .await
            .map_err(|e| to_status(&e))?;

        Ok(Response::new(FetchMasterKeyResponse {
            key_material: Vec::new(), // never sent over gRPC
            epoch: Some(to_proto_epoch(epoch)),
            algorithm: "AES-256-GCM".into(),
            created_at: None,
        }))
    }

    async fn current_epoch(
        &self,
        _request: Request<CurrentEpochRequest>,
    ) -> Result<Response<CurrentEpochResponse>, Status> {
        let _s = kiseki_tracing::span("KeyManagerService.CurrentEpoch");
        let epoch = self.ops.current_epoch().await.map_err(|e| to_status(&e))?;
        let retained: Vec<kiseki_proto::v1::KeyEpoch> = self
            .ops
            .list_epochs()
            .await
            .into_iter()
            .filter(|e| !e.is_current)
            .map(|e| to_proto_epoch(e.epoch))
            .collect();

        Ok(Response::new(CurrentEpochResponse {
            current: Some(to_proto_epoch(epoch)),
            retained,
        }))
    }

    async fn rotate_system_key(
        &self,
        _request: Request<RotateSystemKeyRequest>,
    ) -> Result<Response<RotateSystemKeyResponse>, Status> {
        let _s = kiseki_tracing::span("KeyManagerService.RotateSystemKey");
        let new_epoch = self.ops.rotate().await.map_err(|e| to_status(&e))?;
        Ok(Response::new(RotateSystemKeyResponse {
            new_epoch: Some(to_proto_epoch(new_epoch)),
        }))
    }

    async fn health(
        &self,
        _request: Request<KeyManagerHealthRequest>,
    ) -> Result<Response<KeyManagerHealthResponse>, Status> {
        let _s = kiseki_tracing::span("KeyManagerService.Health");
        let epoch = self.ops.current_epoch().await.map_err(|e| to_status(&e))?;
        Ok(Response::new(KeyManagerHealthResponse {
            current_epoch: Some(to_proto_epoch(epoch)),
            raft_members: Vec::new(), // filled by Raft layer
            leader: 0,                // filled by Raft layer
            healthy: true,
        }))
    }
}
