//! gRPC service implementation for Workflow Advisory.
//!
//! Wraps `WorkflowTable` and `BudgetEnforcer` behind the tonic-generated
//! `WorkflowAdvisoryService` trait. Proto types use `oneof` outcomes —
//! domain errors are returned inside the oneof, not as `tonic::Status`.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use kiseki_common::advisory::{PhaseId, WorkflowRef, WorkloadProfile};
use kiseki_proto::v1::{
    declare_workflow_response, end_workflow_response, get_workflow_status_response,
    phase_advance_response, workflow_advisory_service_server::WorkflowAdvisoryService,
    AdvisoryClientMessage, AdvisoryServerMessage, DeclareWorkflowRequest, DeclareWorkflowResponse,
    DeclareWorkflowSuccess, Empty, EndWorkflowRequest, EndWorkflowResponse,
    GetWorkflowStatusRequest, GetWorkflowStatusResponse, PhaseAdvanceRequest, PhaseAdvanceResponse,
    SubscribeTelemetryRequest, TelemetryEvent, WorkflowCorrelation,
    WorkflowStatus as ProtoWorkflowStatus,
};
use tonic::{Request, Response, Status};

use crate::budget::{BudgetConfig, BudgetEnforcer};
use crate::error::AdvisoryError;
use crate::workflow::WorkflowTable;

// ============================================================================
// Conversion helpers
// ============================================================================

#[allow(clippy::result_large_err)] // tonic::Status is large by design
fn extract_wf_ref(corr: Option<&WorkflowCorrelation>) -> Result<WorkflowRef, Status> {
    let c = corr.ok_or_else(|| Status::invalid_argument("correlation required"))?;
    let proto_ref = c
        .workflow_ref
        .as_ref()
        .ok_or_else(|| Status::invalid_argument("workflow_ref required"))?;
    if proto_ref.handle.len() != 16 {
        return Err(Status::invalid_argument(
            "workflow_ref handle must be 16 bytes",
        ));
    }
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&proto_ref.handle);
    Ok(WorkflowRef(bytes))
}

fn to_proto_ref(wf: &WorkflowRef) -> kiseki_proto::v1::WorkflowRef {
    kiseki_proto::v1::WorkflowRef {
        handle: wf.0.to_vec(),
    }
}

fn domain_error_to_proto(e: &AdvisoryError) -> kiseki_proto::v1::AdvisoryError {
    let code = match e {
        AdvisoryError::WorkflowNotFound => 14,        // ScopeNotFound
        AdvisoryError::BudgetExceeded(_) => 5,        // BudgetExceeded
        AdvisoryError::ProfileNotAllowed(_) => 3,     // ProfileNotAllowed
        AdvisoryError::PhaseNotMonotonic { .. } => 9, // PhaseNotMonotonic
        AdvisoryError::AdvisoryDisabled => 2,         // AdvisoryDisabled
    };
    kiseki_proto::v1::AdvisoryError {
        code,
        message: e.to_string(),
        padding: Vec::new(),
    }
}

#[allow(clippy::result_large_err)]
fn proto_profile_to_domain(val: i32) -> Result<WorkloadProfile, Status> {
    match val {
        1 => Ok(WorkloadProfile::AiTraining),
        2 => Ok(WorkloadProfile::AiInference),
        3 => Ok(WorkloadProfile::HpcCheckpoint),
        4 => Ok(WorkloadProfile::BatchEtl),
        5 => Ok(WorkloadProfile::Interactive),
        _ => Err(Status::invalid_argument("unknown workload profile")),
    }
}

// ============================================================================
// Service implementation
// ============================================================================

/// gRPC handler for the Workflow Advisory service.
pub struct AdvisoryGrpc {
    table: Mutex<WorkflowTable>,
    budget: Arc<Mutex<BudgetEnforcer>>,
}

impl AdvisoryGrpc {
    /// Create a new advisory gRPC handler.
    #[must_use]
    pub fn new(budget_config: BudgetConfig) -> Self {
        Self {
            table: Mutex::new(WorkflowTable::new()),
            budget: Arc::new(Mutex::new(BudgetEnforcer::new(budget_config))),
        }
    }
}

impl AdvisoryGrpc {
    /// Process a single client message: check budget, extract `hint_id`, send ack.
    async fn process_client_message(
        msg: &AdvisoryClientMessage,
        budget: &Arc<Mutex<BudgetEnforcer>>,
        tx: &tokio::sync::mpsc::Sender<Result<AdvisoryServerMessage, Status>>,
    ) {
        use kiseki_proto::v1::advisory_client_message::Payload;
        use kiseki_proto::v1::advisory_server_message::Payload as ServerPayload;
        use kiseki_proto::v1::hint_ack;

        // Extract hint_id for the ack (from Hint or PrefetchHint payloads).
        let hint_id = match msg.payload {
            Some(Payload::Hint(ref h)) => h.hint_id.clone(),
            Some(Payload::Prefetch(ref p)) => p.hint_id.clone(),
            Some(Payload::Heartbeat(_)) => {
                // Heartbeat: no ack needed, just keep-alive.
                tracing::trace!("advisory stream: heartbeat received");
                return;
            }
            Some(Payload::CollectiveAnnouncement(ref c)) => c.hint_id.clone(),
            None => Vec::new(),
        };

        // Budget check.
        let outcome = {
            let mut b = budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            match b.try_hint() {
                Ok(()) => {
                    tracing::debug!("advisory hint accepted");
                    hint_ack::Outcome::Accepted
                }
                Err(ref e) => {
                    tracing::debug!(error = %e, "advisory hint throttled");
                    hint_ack::Outcome::Throttled
                }
            }
        };

        // Send HintAck back (best-effort).
        let ack = AdvisoryServerMessage {
            payload: Some(ServerPayload::HintAck(kiseki_proto::v1::HintAck {
                hint_id,
                outcome: outcome.into(),
                error: None,
            })),
            padding: Vec::new(),
        };
        let _ = tx.send(Ok(ack)).await;
    }
}

#[allow(clippy::result_large_err)] // tonic::Status is large by design
#[tonic::async_trait]
impl WorkflowAdvisoryService for AdvisoryGrpc {
    async fn declare_workflow(
        &self,
        request: Request<DeclareWorkflowRequest>,
    ) -> Result<Response<DeclareWorkflowResponse>, Status> {
        let req = request.into_inner();

        // Budget check.
        {
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Err(e) = budget.try_declare() {
                return Ok(Response::new(DeclareWorkflowResponse {
                    outcome: Some(declare_workflow_response::Outcome::Error(
                        domain_error_to_proto(&e),
                    )),
                }));
            }
        }

        let profile = proto_profile_to_domain(req.profile)?;
        let phase = PhaseId(req.initial_phase_id);

        // Generate workflow ref via UUID.
        let handle = uuid::Uuid::new_v4().into_bytes();
        let wf_ref = WorkflowRef(handle);

        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        table.declare(wf_ref, profile, phase);

        Ok(Response::new(DeclareWorkflowResponse {
            outcome: Some(declare_workflow_response::Outcome::Success(
                DeclareWorkflowSuccess {
                    workflow_ref: Some(to_proto_ref(&wf_ref)),
                    available_pools: Vec::new(),
                },
            )),
        }))
    }

    async fn end_workflow(
        &self,
        request: Request<EndWorkflowRequest>,
    ) -> Result<Response<EndWorkflowResponse>, Status> {
        let req = request.into_inner();
        let wf_ref = extract_wf_ref(req.correlation.as_ref())?;

        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let ended = table.end(&wf_ref);

        if ended {
            let mut budget = self
                .budget
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            budget.release_workflow();
        }

        Ok(Response::new(EndWorkflowResponse {
            outcome: Some(end_workflow_response::Outcome::Ok(Empty {})),
        }))
    }

    async fn phase_advance(
        &self,
        request: Request<PhaseAdvanceRequest>,
    ) -> Result<Response<PhaseAdvanceResponse>, Status> {
        let req = request.into_inner();
        let wf_ref = extract_wf_ref(req.correlation.as_ref())?;

        let mut table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = table
            .get_mut(&wf_ref)
            .ok_or_else(|| Status::not_found("workflow not found"))?;

        if let Err(e) = entry.advance_phase(PhaseId(req.next_phase_id)) {
            return Ok(Response::new(PhaseAdvanceResponse {
                outcome: Some(phase_advance_response::Outcome::Error(
                    domain_error_to_proto(&e),
                )),
            }));
        }

        Ok(Response::new(PhaseAdvanceResponse {
            outcome: Some(phase_advance_response::Outcome::Ok(Empty {})),
        }))
    }

    async fn get_workflow_status(
        &self,
        request: Request<GetWorkflowStatusRequest>,
    ) -> Result<Response<GetWorkflowStatusResponse>, Status> {
        let req = request.into_inner();
        let wf_ref = extract_wf_ref(req.correlation.as_ref())?;

        let table = self
            .table
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let entry = table
            .get(&wf_ref)
            .ok_or_else(|| Status::not_found("workflow not found"))?;

        Ok(Response::new(GetWorkflowStatusResponse {
            outcome: Some(get_workflow_status_response::Outcome::Status(
                ProtoWorkflowStatus {
                    current_phase_id: entry.current_phase.0,
                    current_phase_tag: String::new(), // tag not tracked in domain yet
                    hints_accepted_last_min: 0,
                    hints_rejected_last_min: 0,
                    padding: Vec::new(),
                },
            )),
        }))
    }

    type AdvisoryStreamStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<AdvisoryServerMessage, Status>> + Send>>;

    async fn advisory_stream(
        &self,
        request: Request<tonic::Streaming<AdvisoryClientMessage>>,
    ) -> Result<Response<Self::AdvisoryStreamStream>, Status> {
        let mut in_stream = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<AdvisoryServerMessage, Status>>(64);

        // Validate first message to bind the stream to a workflow.
        // Subsequent messages must carry the same workflow_ref.
        let first_msg = in_stream
            .message()
            .await
            .map_err(|e| Status::internal(format!("stream read error: {e}")))?
            .ok_or_else(|| Status::invalid_argument("empty stream — expected initial message"))?;

        let bound_wf_ref = extract_wf_ref(
            first_msg
                .correlation
                .as_ref()
                .map(|c| c as &WorkflowCorrelation),
        )?;

        // Verify the workflow exists.
        {
            let tbl = self
                .table
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if tbl.get(&bound_wf_ref).is_none() {
                return Err(Status::not_found("workflow not found"));
            }
        }

        // Process the first message's hint.
        let budget = Arc::clone(&self.budget);
        Self::process_client_message(&first_msg, &budget, &tx).await;

        // Spawn a task to process subsequent incoming hints.
        tokio::spawn(async move {
            while let Ok(Some(msg)) = in_stream.message().await {
                // Verify correlation matches the bound workflow (defense-in-depth, I-WA3).
                if let Ok(msg_ref) =
                    extract_wf_ref(msg.correlation.as_ref().map(|c| c as &WorkflowCorrelation))
                {
                    if msg_ref.0 != bound_wf_ref.0 {
                        tracing::debug!("advisory stream: correlation mismatch, ignoring message");
                        let _ = tx
                            .send(Ok(AdvisoryServerMessage {
                                payload: Some(
                                    kiseki_proto::v1::advisory_server_message::Payload::Warning(
                                        kiseki_proto::v1::StreamWarning { kind: 0 },
                                    ),
                                ),
                                padding: Vec::new(),
                            }))
                            .await;
                        continue;
                    }
                }

                Self::process_client_message(&msg, &budget, &tx).await;
            }
            tracing::debug!("advisory stream closed by client");
        });

        let out_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out_stream)))
    }

    type SubscribeTelemetryStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<TelemetryEvent, Status>> + Send>>;

    async fn subscribe_telemetry(
        &self,
        _request: Request<SubscribeTelemetryRequest>,
    ) -> Result<Response<Self::SubscribeTelemetryStream>, Status> {
        Err(Status::unimplemented("telemetry not yet implemented"))
    }
}
