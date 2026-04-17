//! Client library for communicating with the vm-pool service.
//!
//! Provides a high-level async API over the Unix socket protocol.
//! The client is generic over an [`AppProtocol`](vm_pool_protocol::AppProtocol)
//! — specify the protocol that matches the service the client is talking to.
//!
//! # Example
//!
//! ```no_run
//! # async fn example() -> Result<(), vm_pool_client::ClientError> {
//! use vm_pool_client::Client;
//! use vm_pool_protocol::{NullProtocol, VmConfig};
//!
//! let mut client: Client<NullProtocol> =
//!     Client::connect("/tmp/vm-pool.sock").await?;
//!
//! let status = client.status().await?;
//! println!("available: {}", status.available);
//!
//! let vm_id = client.allocate("agent:v1.0.0", VmConfig::default()).await?;
//! println!("allocated: {}", vm_id);
//!
//! client.deallocate(&vm_id).await?;
//! # Ok(())
//! # }
//! ```

use std::marker::PhantomData;
use std::path::Path;

use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tracing::debug;
use vm_pool_protocol::{
    AppProtocol, LogLine, NullProtocol, ServiceCommand, ServiceEvent, VmConfig, VmId,
};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("connection failed: {0}")]
    Connect(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("connection closed")]
    Closed,
    #[error("service error: {0}")]
    Service(String),
    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}

/// Pool status information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub total: usize,
    pub available: usize,
    pub allocated: usize,
}

/// Client for communicating with the vm-pool service.
pub struct Client<P: AppProtocol = NullProtocol> {
    /// Channel for sending serialized commands to the writer task.
    cmd_tx: mpsc::Sender<String>,
    /// Channel for receiving parsed responses from the reader task.
    resp_rx: mpsc::Receiver<ServiceEvent<P>>,
    _marker: PhantomData<P>,
}

impl<P: AppProtocol> Client<P> {
    /// Connect to the vm-pool service at the given Unix socket path.
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let stream = UnixStream::connect(path.as_ref()).await?;
        let (reader, writer) = stream.into_split();

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<String>(64);
        let (resp_tx, resp_rx) = mpsc::channel::<ServiceEvent<P>>(64);

        // Writer task
        tokio::spawn(async move {
            let mut writer = writer;
            while let Some(line) = cmd_rx.recv().await {
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                if writer.write_all(b"\n").await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
            }
        });

        // Reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        if let Ok(event) =
                            serde_json::from_str::<ServiceEvent<P>>(line.trim())
                        {
                            if resp_tx.send(event).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            cmd_tx,
            resp_rx,
            _marker: PhantomData,
        })
    }

    /// Send a command and wait for the next response.
    async fn request(
        &mut self,
        command: ServiceCommand<P>,
    ) -> Result<ServiceEvent<P>, ClientError> {
        let json = serde_json::to_string(&command)?;
        debug!("sending: {}", json);
        self.cmd_tx
            .send(json)
            .await
            .map_err(|_| ClientError::Closed)?;

        self.resp_rx.recv().await.ok_or(ClientError::Closed)
    }

    /// Convert a ServiceEvent::Error into a ClientError, or return the event.
    fn check_error(event: ServiceEvent<P>) -> Result<ServiceEvent<P>, ClientError> {
        match event {
            ServiceEvent::Error { message } => Err(ClientError::Service(message)),
            other => Ok(other),
        }
    }

    /// Get pool status.
    pub async fn status(&mut self) -> Result<PoolStatus, ClientError> {
        let resp = self.request(ServiceCommand::Status).await?;
        match Self::check_error(resp)? {
            ServiceEvent::PoolStatus {
                total,
                available,
                allocated,
            } => Ok(PoolStatus {
                total,
                available,
                allocated,
            }),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Allocate a new VM. Returns the VM ID.
    pub async fn allocate(
        &mut self,
        image: &str,
        config: VmConfig,
    ) -> Result<VmId, ClientError> {
        let resp = self
            .request(ServiceCommand::Allocate {
                image: image.to_string(),
                config,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmAllocated { vm_id, .. } => Ok(vm_id),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Deallocate a VM.
    pub async fn deallocate(&mut self, vm_id: &VmId) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Deallocate {
                vm_id: vm_id.clone(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmStopped { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Send an application command to a VM.
    ///
    /// Returns when the service acks forwarding (CommandSent). The VM's
    /// application response events arrive asynchronously via `next_event`.
    pub async fn send_to_vm(
        &mut self,
        vm_id: &VmId,
        command: P::Command,
    ) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Send {
                vm_id: vm_id.clone(),
                command,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::CommandSent { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Save a snapshot of a VM.
    pub async fn snapshot(&mut self, vm_id: &VmId, name: &str) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Snapshot {
                vm_id: vm_id.clone(),
                name: name.to_string(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmStopped { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Restore a VM from a snapshot.
    pub async fn restore(&mut self, vm_id: &VmId, snapshot: &str) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::Restore {
                vm_id: vm_id.clone(),
                snapshot: snapshot.to_string(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::VmReady { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Tail log lines from a VM.
    pub async fn tail_logs(
        &mut self,
        vm_id: &VmId,
        lines: usize,
    ) -> Result<Vec<LogLine>, ClientError> {
        let resp = self
            .request(ServiceCommand::TailLogs {
                vm_id: vm_id.clone(),
                lines,
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogTail { lines, .. } => Ok(lines),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Subscribe to logs from a specific VM (or all VMs if None).
    pub async fn subscribe_logs(
        &mut self,
        vm_id: Option<&VmId>,
    ) -> Result<(), ClientError> {
        let resp = self
            .request(ServiceCommand::SubscribeLogs {
                vm_id: vm_id.cloned(),
            })
            .await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogsSubscribed { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Unsubscribe from log streaming.
    pub async fn unsubscribe_logs(&mut self) -> Result<(), ClientError> {
        let resp = self.request(ServiceCommand::UnsubscribeLogs).await?;
        match Self::check_error(resp)? {
            ServiceEvent::LogsSubscribed { .. } => Ok(()),
            other => Err(ClientError::UnexpectedResponse(format!("{other:?}"))),
        }
    }

    /// Receive the next event (for streaming/subscriptions).
    /// Returns None if the connection is closed.
    pub async fn next_event(&mut self) -> Option<ServiceEvent<P>> {
        self.resp_rx.recv().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use vm_pool_manager::{NoRuntime, PoolConfig};
    use vm_pool_protocol::ShellProtocol;
    use vm_pool_service::{Service, ServiceConfig};

    type TestClient = Client<ShellProtocol>;
    type TestService = Service<NoRuntime, ShellProtocol>;

    /// Start a service on a temp socket and return a connected client.
    async fn test_client() -> (TestClient, Arc<TestService>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let config = ServiceConfig {
            socket_path: socket_path.clone(),
            snapshot_dir: dir.path().join("snapshots"),
            pool: PoolConfig {
                max_vms: 3,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
        };

        let service = TestService::new(config).await.unwrap();
        let svc = service.clone();

        // Run service in background
        tokio::spawn(async move { svc.run().await });

        // Wait for socket to be ready
        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = Client::<ShellProtocol>::connect(&socket_path).await.unwrap();
        (client, service, dir)
    }

    #[tokio::test]
    async fn client_status() {
        let (mut client, _svc, _dir) = test_client().await;

        let status = client.status().await.unwrap();
        assert_eq!(status.total, 3);
        assert_eq!(status.available, 3);
        assert_eq!(status.allocated, 0);
    }

    #[tokio::test]
    async fn client_allocate_and_deallocate() {
        let (mut client, _svc, _dir) = test_client().await;

        let vm_id = client
            .allocate("agent:v1", VmConfig::default())
            .await
            .unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 1);

        client.deallocate(&vm_id).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 0);
    }

    #[tokio::test]
    async fn client_allocate_error() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("test.sock");

        let config = ServiceConfig {
            socket_path: socket_path.clone(),
            snapshot_dir: dir.path().join("snapshots"),
            pool: PoolConfig {
                max_vms: 0, // No VMs allowed
                health_check_interval: 300,
                vm_timeout: 7200,
            },
        };

        let service = TestService::new(config).await.unwrap();
        let svc = service.clone();
        tokio::spawn(async move { svc.run().await });

        for _ in 0..50 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let mut client = Client::<ShellProtocol>::connect(&socket_path).await.unwrap();
        let result = client.allocate("agent:v1", VmConfig::default()).await;
        assert!(matches!(result, Err(ClientError::Service(_))));
    }

    #[tokio::test]
    async fn client_tail_logs() {
        let (mut client, _svc, _dir) = test_client().await;

        let vm_id = VmId::new("vm-nonexistent");
        let logs = client.tail_logs(&vm_id, 10).await.unwrap();
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn client_subscribe_unsubscribe() {
        let (mut client, _svc, _dir) = test_client().await;

        client.subscribe_logs(None).await.unwrap();
        client.unsubscribe_logs().await.unwrap();
    }

    #[tokio::test]
    async fn client_full_lifecycle() {
        let (mut client, _svc, _dir) = test_client().await;

        // Check initial status
        let status = client.status().await.unwrap();
        assert_eq!(status.available, 3);

        // Allocate two VMs
        let vm1 = client
            .allocate("agent:v1", VmConfig::default())
            .await
            .unwrap();
        let vm2 = client
            .allocate("automation:v1", VmConfig::default())
            .await
            .unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 2);
        assert_eq!(status.available, 1);

        // Deallocate one
        client.deallocate(&vm1).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 1);
        assert_eq!(status.available, 2);

        // Deallocate the other
        client.deallocate(&vm2).await.unwrap();

        let status = client.status().await.unwrap();
        assert_eq!(status.allocated, 0);
    }
}
