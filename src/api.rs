use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::net::UnixStream;
use tonic::transport::{Channel, Endpoint, Uri};
use tower::service_fn;

use crate::pb;
use crate::pb::kilnd_service_client::KilndServiceClient;

/// A backend-neutral sandbox request expressed in kilnd API semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxRequest {
    pub image: String,
    pub memory_bytes: u64,
    pub cpu_millis: u32,
    pub pids: u32,
    pub disk_bytes: u64,
    pub network_none: bool,
    pub read_only_root: bool,
    pub writable_paths: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    pub id: String,
    pub backend_id: String,
    pub labels: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxExec {
    pub sandbox_id: String,
    pub argv: Vec<String>,
    pub stdin: Vec<u8>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionState {
    Exited,
    TimedOut,
    Canceled,
    Failed,
    Interrupted,
    Unknown,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CapturedOutput {
    pub data: Vec<u8>,
    pub observed_bytes: u64,
    pub total_bytes_known: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Execution {
    pub id: String,
    pub state: ExecutionState,
    pub exit_code: Option<i32>,
    pub failure_reason: String,
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

impl Execution {
    #[must_use]
    pub fn succeeded(&self) -> bool {
        self.state == ExecutionState::Exited && self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeReservations {
    pub sandboxes: u64,
    pub memory_bytes: u64,
    pub pids: u64,
    pub disk_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub serving: bool,
    pub detail: String,
    pub active_sandboxes: u32,
    pub running_executions: u32,
    pub reservations: NodeReservations,
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[error("kilnd {code}: {message}")]
pub struct ApiError {
    pub code: tonic::Code,
    pub message: String,
}

impl ApiError {
    fn local(message: impl Into<String>) -> Self {
        Self {
            // Local transport/protocol failures cannot prove create rejection.
            code: tonic::Code::Unknown,
            message: message.into(),
        }
    }
}

impl From<tonic::Status> for ApiError {
    fn from(status: tonic::Status) -> Self {
        Self {
            code: status.code(),
            message: status.message().to_string(),
        }
    }
}

#[async_trait]
pub trait SandboxApi: Send + Sync {
    async fn health(&self) -> Result<Health, ApiError>;
    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError>;
    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError>;
    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError>;
    /// Request idempotent deletion. Success acknowledges a deleted response state;
    /// callers must independently confirm absence via `non_deleted_sandboxes`.
    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError>;
    /// Independently list every sandbox not yet deleted, including creating,
    /// deleting, and failed sandboxes. Errors or incomplete pagination must not
    /// be returned as a successful empty listing.
    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError>;
}

#[derive(Clone)]
pub struct TonicKilnd {
    client: KilndServiceClient<Channel>,
}

impl TonicKilnd {
    /// Connect a client channel to kilnd's Unix socket.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be configured or connected.
    pub async fn connect(path: PathBuf) -> Result<Self, ApiError> {
        let shown = path.display().to_string();
        let channel = Endpoint::try_from("http://kilnd.invalid")
            .map_err(|error| ApiError::local(error.to_string()))?
            .connect_with_connector(service_fn(move |_: Uri| {
                let path = path.clone();
                async move {
                    UnixStream::connect(path)
                        .await
                        .map(TokioIo::new)
                        .map_err(std::io::Error::other)
                }
            }))
            .await
            .map_err(|error| ApiError::local(format!("connect to {shown}: {error}")))?;
        Ok(Self {
            client: KilndServiceClient::new(channel).max_decoding_message_size(4 << 20),
        })
    }

    fn sandbox(message: Option<pb::Sandbox>) -> Result<Sandbox, ApiError> {
        let sandbox = message.ok_or_else(|| ApiError::local("response omitted sandbox"))?;
        Ok(Sandbox {
            id: sandbox.id,
            backend_id: sandbox.backend_id,
            labels: sandbox
                .spec
                .map_or_else(BTreeMap::new, |spec| spec.labels.into_iter().collect()),
        })
    }
}

#[async_trait]
impl SandboxApi for TonicKilnd {
    async fn health(&self) -> Result<Health, ApiError> {
        let response = self
            .client
            .clone()
            .health(pb::HealthRequest {})
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let resources = response.node_resources.unwrap_or_default();
        Ok(Health {
            serving: response.status == pb::health_response::Status::Serving as i32,
            detail: response.detail,
            active_sandboxes: response.active_sandboxes,
            running_executions: response.running_executions,
            reservations: NodeReservations {
                sandboxes: resources.reserved_sandboxes,
                memory_bytes: resources.reserved_memory_bytes,
                pids: resources.reserved_pids,
                disk_bytes: resources.reserved_disk_bytes,
            },
        })
    }

    async fn create(&self, request: SandboxRequest) -> Result<Sandbox, ApiError> {
        if !request.network_none {
            return Err(ApiError::local(
                "this milestone accepts only network-none sandboxes",
            ));
        }
        let response = self
            .client
            .clone()
            .create_sandbox(pb::CreateSandboxRequest {
                spec: Some(pb::SandboxSpec {
                    image: request.image,
                    limits: Some(pb::ResourceLimits {
                        memory_bytes: request.memory_bytes,
                        cpu_millis: request.cpu_millis,
                        pids: request.pids,
                        disk_bytes: request.disk_bytes,
                    }),
                    network: Some(pb::NetworkPolicy {
                        mode: pb::NetworkMode::None as i32,
                        allow: Vec::new(),
                    }),
                    read_only_root: request.read_only_root,
                    writable_paths: request.writable_paths,
                    labels: request.labels.into_iter().collect(),
                }),
                expiry: None,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        Self::sandbox(response.sandbox)
    }

    async fn exec(&self, request: SandboxExec) -> Result<Execution, ApiError> {
        let timeout_ms = u32::try_from(request.timeout.as_millis())
            .map_err(|_| ApiError::local("execution timeout exceeds kilnd uint32 limit"))?;
        let response = self
            .client
            .clone()
            .exec(pb::ExecRequest {
                sandbox_id: request.sandbox_id,
                argv: request.argv,
                stdin: request.stdin,
                timeout_ms,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let execution = response
            .execution
            .ok_or_else(|| ApiError::local("response omitted execution"))?;
        Ok(execution_from_proto(execution))
    }

    async fn recent_executions(&self, sandbox_id: &str) -> Result<Vec<Execution>, ApiError> {
        let response = self
            .client
            .clone()
            .get_sandbox(pb::GetSandboxRequest {
                id: sandbox_id.to_string(),
                execution_limit: 100,
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        Ok(response
            .recent_executions
            .into_iter()
            .map(execution_from_proto)
            .collect())
    }

    async fn delete(&self, sandbox_id: &str) -> Result<(), ApiError> {
        let response = self
            .client
            .clone()
            .delete_sandbox(pb::DeleteSandboxRequest {
                id: sandbox_id.to_string(),
            })
            .await
            .map_err(ApiError::from)?
            .into_inner();
        let sandbox = response
            .sandbox
            .ok_or_else(|| ApiError::local("delete response omitted sandbox"))?;
        if sandbox.state != pb::SandboxState::Deleted as i32 {
            return Err(ApiError::local(format!(
                "delete returned non-deleted state {}",
                sandbox.state
            )));
        }
        Ok(())
    }

    async fn non_deleted_sandboxes(&self) -> Result<Vec<Sandbox>, ApiError> {
        let mut page_token = String::new();
        let mut sandboxes = Vec::new();
        loop {
            let response = self
                .client
                .clone()
                .list_sandboxes(pb::ListSandboxesRequest {
                    include_deleted: false,
                    page_size: Some(0),
                    page_token,
                })
                .await
                .map_err(ApiError::from)?
                .into_inner();
            for sandbox in response.sandboxes {
                sandboxes.push(Sandbox {
                    id: sandbox.id,
                    backend_id: sandbox.backend_id,
                    labels: sandbox
                        .spec
                        .map_or_else(BTreeMap::new, |spec| spec.labels.into_iter().collect()),
                });
            }
            page_token = response.next_page_token;
            if page_token.is_empty() {
                break;
            }
        }
        Ok(sandboxes)
    }
}

fn output(output: Option<pb::Output>) -> CapturedOutput {
    output.map_or_else(CapturedOutput::default, |output| CapturedOutput {
        data: output.data,
        observed_bytes: output.total_bytes,
        total_bytes_known: output.total_bytes_known,
        truncated: output.truncated,
    })
}

fn execution_from_proto(execution: pb::Execution) -> Execution {
    Execution {
        id: execution.id,
        state: match pb::ExecutionState::try_from(execution.state) {
            Ok(pb::ExecutionState::Exited) => ExecutionState::Exited,
            Ok(pb::ExecutionState::TimedOut) => ExecutionState::TimedOut,
            Ok(pb::ExecutionState::Canceled) => ExecutionState::Canceled,
            Ok(pb::ExecutionState::Failed) => ExecutionState::Failed,
            Ok(pb::ExecutionState::Interrupted) => ExecutionState::Interrupted,
            Ok(pb::ExecutionState::Unspecified | pb::ExecutionState::Running) | Err(_) => {
                ExecutionState::Unknown
            }
        },
        exit_code: execution.exit_code,
        failure_reason: execution.failure_reason,
        stdout: output(execution.stdout),
        stderr: output(execution.stderr),
    }
}

#[cfg(test)]
mod status_contract_test {
    use super::ApiError;
    use tonic::{Code, Status};

    #[test]
    fn tonic_status_codes_survive_adapter_conversion() {
        for code in [
            Code::InvalidArgument,
            Code::ResourceExhausted,
            Code::FailedPrecondition,
            Code::PermissionDenied,
            Code::Unauthenticated,
        ] {
            let error = ApiError::from(Status::new(code, "rejected"));
            assert_eq!(error.code, code);
            assert_eq!(error.message, "rejected");
        }
        assert_eq!(ApiError::local("response lost").code, Code::Unknown);
    }
}
