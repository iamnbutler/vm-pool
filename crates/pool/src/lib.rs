//! VM pool: allocation, limits, health monitoring, lifecycle.
//!
//! The pool manages VM instances through a [`VmRuntime`] trait that
//! abstracts the container backend. [`ContainerRuntime`] provides the
//! real implementation using apple/container.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use vm_pool_events::{EventLog, EventPayload, VmState};
use vm_pool_images::ImageRef;
use vm_pool_protocol::{VmCommand, VmConfig, VmEvent, VmId};
use vm_pool_transport::{TransportError, VmTransport};
use thiserror::Error;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};

#[derive(Debug, Error)]
pub enum PoolError {
    #[error("pool exhausted: {available} available, {requested} requested")]
    Exhausted { available: usize, requested: usize },
    #[error("VM not found: {0}")]
    VmNotFound(VmId),
    #[error("VM not ready: {0}")]
    VmNotReady(VmId),
    #[error("image error: {0}")]
    Image(#[from] vm_pool_images::ImageError),
    #[error("transport error: {0}")]
    Transport(#[from] TransportError),
    #[error("runtime error: {0}")]
    Runtime(String),
}

/// Handle to a running VM, providing command/event channels.
pub struct VmHandle {
    pub command_tx: mpsc::Sender<VmCommand>,
    pub event_rx: mpsc::Receiver<VmEvent>,
}

/// Trait abstracting the VM container backend.
pub trait VmRuntime: Send + Sync + 'static {
    fn start(
        &self,
        vm_id: &VmId,
        image: &ImageRef,
        config: &VmConfig,
    ) -> impl Future<Output = Result<VmHandle, PoolError>> + Send;

    fn stop(&self, vm_id: &VmId) -> impl Future<Output = Result<(), PoolError>> + Send;
}

/// Real container runtime using apple/container CLI.
///
/// Starts VMs with `container run -i` and communicates via the
/// supervisor binary over JSON-line stdio.
pub struct ContainerRuntime {
    /// Transports keyed by VM ID, for stopping.
    transports: RwLock<HashMap<VmId, ()>>,
}

impl ContainerRuntime {
    pub fn new() -> Self {
        Self {
            transports: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for ContainerRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl VmRuntime for ContainerRuntime {
    async fn start(
        &self,
        vm_id: &VmId,
        image: &ImageRef,
        config: &VmConfig,
    ) -> Result<VmHandle, PoolError> {
        let image_tag = image.to_string();

        let cpus = config.cpus.unwrap_or(2).to_string();
        let memory = format!("{}M", config.memory_mb.unwrap_or(2048));

        let mut args: Vec<String> = vec![
            "run".into(),
            "--rm".into(),
            "-i".into(),
            "--name".into(),
            vm_id.as_str().into(),
            "--cpus".into(),
            cpus,
            "--memory".into(),
            memory,
        ];

        for (key, value) in &config.env {
            args.push("-e".into());
            args.push(format!("{}={}", key, value));
        }

        args.push(image_tag);

        info!(%vm_id, ?args, "starting container");

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
        let mut transport = VmTransport::spawn("container", &args_refs)
            .await
            .map_err(|e| PoolError::Runtime(format!("failed to spawn container: {e}")))?;

        // Wait for Ready event from supervisor
        let first_event = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            transport.recv(),
        )
        .await
        .map_err(|_| PoolError::Runtime("timeout waiting for supervisor Ready".into()))?
        .ok_or_else(|| PoolError::Runtime("transport closed before Ready".into()))?;

        if first_event != VmEvent::Ready {
            return Err(PoolError::Runtime(format!(
                "expected Ready, got {:?}",
                first_event
            )));
        }

        // Set up command forwarding channels
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<VmCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<VmEvent>(64);

        // Bridge task: forward commands to transport, events from transport
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(command) => {
                                if let Err(e) = transport.send(&command).await {
                                    error!("failed to send command: {}", e);
                                    break;
                                }
                            }
                            None => {
                                // Command channel closed — shut down
                                let _ = transport.send(&VmCommand::Shutdown).await;
                                break;
                            }
                        }
                    }
                    event = transport.recv() => {
                        match event {
                            Some(evt) => {
                                if evt_tx.send(evt).await.is_err() {
                                    break;
                                }
                            }
                            None => {
                                // Transport closed
                                break;
                            }
                        }
                    }
                }
            }
            // Ensure cleanup
            let _ = transport.close().await;
        });

        self.transports.write().await.insert(vm_id.clone(), ());

        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        self.transports.write().await.remove(vm_id);

        // Also stop via CLI in case the process is still running
        let output = tokio::process::Command::new("container")
            .args(["stop", vm_id.as_str()])
            .output()
            .await;

        match output {
            Ok(o) if o.status.success() => {
                info!(%vm_id, "container stopped");
            }
            Ok(o) => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                debug!(%vm_id, %stderr, "container stop returned non-zero (may already be gone)");
            }
            Err(e) => {
                warn!(%vm_id, error = %e, "failed to run container stop");
            }
        }

        Ok(())
    }
}

/// A runtime that uses the supervisor binary directly (no container).
/// Useful for testing the full pool flow without needing container images.
pub struct SupervisorRuntime {
    supervisor_path: std::path::PathBuf,
}

impl SupervisorRuntime {
    pub fn new(supervisor_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            supervisor_path: supervisor_path.into(),
        }
    }
}

impl VmRuntime for SupervisorRuntime {
    async fn start(
        &self,
        vm_id: &VmId,
        _image: &ImageRef,
        _config: &VmConfig,
    ) -> Result<VmHandle, PoolError> {
        let path = self.supervisor_path.to_str().ok_or_else(|| {
            PoolError::Runtime("supervisor path is not valid UTF-8".into())
        })?;

        let mut transport = VmTransport::spawn(path, &[])
            .await
            .map_err(|e| PoolError::Runtime(format!("failed to spawn supervisor: {e}")))?;

        // Wait for Ready
        let first_event = transport
            .recv()
            .await
            .ok_or_else(|| PoolError::Runtime("transport closed before Ready".into()))?;

        if first_event != VmEvent::Ready {
            return Err(PoolError::Runtime(format!(
                "expected Ready, got {:?}",
                first_event
            )));
        }

        let (cmd_tx, mut cmd_rx) = mpsc::channel::<VmCommand>(64);
        let (evt_tx, evt_rx) = mpsc::channel::<VmEvent>(64);

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(command) => {
                                if let Err(e) = transport.send(&command).await {
                                    error!("failed to send command: {}", e);
                                    break;
                                }
                            }
                            None => {
                                let _ = transport.send(&VmCommand::Shutdown).await;
                                break;
                            }
                        }
                    }
                    event = transport.recv() => {
                        match event {
                            Some(evt) => {
                                if evt_tx.send(evt).await.is_err() {
                                    break;
                                }
                            }
                            None => break,
                        }
                    }
                }
            }
            let _ = transport.close().await;
        });

        info!(%vm_id, path = %self.supervisor_path.display(), "supervisor started");

        Ok(VmHandle {
            command_tx: cmd_tx,
            event_rx: evt_rx,
        })
    }

    async fn stop(&self, vm_id: &VmId) -> Result<(), PoolError> {
        debug!(%vm_id, "supervisor stop (channel drop handles shutdown)");
        Ok(())
    }
}

/// Configuration for the VM pool.
#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub max_vms: usize,
    pub health_check_interval: u64,
    pub vm_timeout: u64,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            max_vms: 6,
            health_check_interval: 30,
            vm_timeout: 7200,
        }
    }
}

/// State of a VM in the pool.
struct VmEntry {
    #[allow(dead_code)]
    id: VmId,
    #[allow(dead_code)]
    image: ImageRef,
    config: VmConfig,
    state: VmState,
    started_at: u64,
    command_tx: Option<mpsc::Sender<VmCommand>>,
}

/// Marker type for pools without a runtime.
pub struct NoRuntime;

/// The VM pool manager, generic over the runtime backend.
pub struct Pool<R = NoRuntime> {
    config: PoolConfig,
    vms: RwLock<HashMap<VmId, VmEntry>>,
    events: Arc<EventLog>,
    runtime: R,
}

impl Pool<NoRuntime> {
    /// Create a pool without a runtime (commands to VMs will return VmNotReady).
    pub fn new(config: PoolConfig, events: Arc<EventLog>) -> Arc<Self> {
        Arc::new(Self {
            config,
            vms: RwLock::new(HashMap::new()),
            events,
            runtime: NoRuntime,
        })
    }
}

impl<R> Pool<R> {
    pub fn with_runtime(config: PoolConfig, events: Arc<EventLog>, runtime: R) -> Arc<Self> {
        Arc::new(Self {
            config,
            vms: RwLock::new(HashMap::new()),
            events,
            runtime,
        })
    }

    pub async fn status(&self) -> PoolStatus {
        let vms = self.vms.read().await;
        PoolStatus {
            total: self.config.max_vms,
            allocated: vms.len(),
            available: self.config.max_vms.saturating_sub(vms.len()),
        }
    }

    pub async fn get(&self, vm_id: &VmId) -> Option<VmState> {
        let vms = self.vms.read().await;
        vms.get(vm_id).map(|e| e.state)
    }

    pub async fn list(&self) -> Vec<(VmId, VmState)> {
        let vms = self.vms.read().await;
        vms.iter().map(|(id, e)| (id.clone(), e.state)).collect()
    }

    pub async fn send_command(
        &self,
        vm_id: &VmId,
        command: VmCommand,
    ) -> Result<(), PoolError> {
        let vms = self.vms.read().await;
        let entry = vms
            .get(vm_id)
            .ok_or_else(|| PoolError::VmNotFound(vm_id.clone()))?;

        match &entry.command_tx {
            Some(tx) => tx.send(command).await.map_err(|_| {
                PoolError::Runtime(format!("VM {} command channel closed", vm_id))
            }),
            None => Err(PoolError::VmNotReady(vm_id.clone())),
        }
    }
}

// Pool without runtime — no transport channels.
impl Pool<NoRuntime> {
    pub async fn allocate(
        &self,
        image: ImageRef,
        config: VmConfig,
    ) -> Result<VmId, PoolError> {
        let mut vms = self.vms.write().await;

        if vms.len() >= self.config.max_vms {
            return Err(PoolError::Exhausted {
                available: 0,
                requested: 1,
            });
        }

        let vm_id = generate_vm_id();
        info!(%vm_id, image = %image, "allocating VM (no runtime)");

        self.events.init_vm(&vm_id).await;
        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Allocating,
            })
            .await;

        let entry = VmEntry {
            id: vm_id.clone(),
            image,
            config,
            state: VmState::Ready,
            started_at: now_ms(),
            command_tx: None,
        };
        vms.insert(vm_id.clone(), entry);

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Ready,
            })
            .await;

        Ok(vm_id)
    }

    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), PoolError> {
        let mut vms = self.vms.write().await;
        if vms.remove(vm_id).is_none() {
            return Err(PoolError::VmNotFound(vm_id.clone()));
        }
        info!(%vm_id, "deallocated VM");

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopped,
            })
            .await;
        self.events.cleanup_vm(vm_id).await;
        Ok(())
    }

    pub async fn health_check(&self) {
        let mut timed_out = Vec::new();
        {
            let vms = self.vms.read().await;
            for (vm_id, entry) in vms.iter() {
                let age_ms = now_ms().saturating_sub(entry.started_at);
                let age_s = age_ms / 1000;
                if age_ms >= self.config.vm_timeout * 1000 {
                    warn!(%vm_id, age_s, timeout = self.config.vm_timeout, "VM exceeded timeout");
                    timed_out.push(vm_id.clone());
                }
                debug!(%vm_id, state = ?entry.state, age_s, "health check");
            }
        }
        for vm_id in timed_out {
            if let Err(e) = self.deallocate(&vm_id).await {
                warn!(%vm_id, error = %e, "failed to deallocate timed-out VM");
            }
        }
    }
}

// Pool with a real runtime — VMs get transport channels.
impl<R: VmRuntime> Pool<R> {
    pub async fn allocate(
        &self,
        image: ImageRef,
        config: VmConfig,
    ) -> Result<VmId, PoolError> {
        let mut vms = self.vms.write().await;

        if vms.len() >= self.config.max_vms {
            return Err(PoolError::Exhausted {
                available: 0,
                requested: 1,
            });
        }

        let vm_id = generate_vm_id();
        info!(%vm_id, image = %image, "allocating VM");

        self.events.init_vm(&vm_id).await;
        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Allocating,
            })
            .await;

        let handle = match self.runtime.start(&vm_id, &image, &config).await {
            Ok(h) => h,
            Err(e) => {
                error!(%vm_id, error = %e, "failed to start VM");
                self.events
                    .append(EventPayload::VmLifecycle {
                        vm_id: vm_id.clone(),
                        state: VmState::Crashed,
                    })
                    .await;
                return Err(e);
            }
        };

        let events = self.events.clone();
        let fwd_vm_id = vm_id.clone();
        tokio::spawn(forward_vm_events(fwd_vm_id, handle.event_rx, events));

        let entry = VmEntry {
            id: vm_id.clone(),
            image,
            config,
            state: VmState::Ready,
            started_at: now_ms(),
            command_tx: Some(handle.command_tx),
        };
        vms.insert(vm_id.clone(), entry);

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Ready,
            })
            .await;

        Ok(vm_id)
    }

    /// Allocate a VM, evicting the lowest-priority VM if the pool is full.
    ///
    /// Returns `(new_vm_id, Option<evicted_vm_id>)`. Fails if the pool is full
    /// and no VM has a strictly lower priority than the requested config.
    pub async fn allocate_or_evict(
        &self,
        image: ImageRef,
        config: VmConfig,
    ) -> Result<(VmId, Option<VmId>), PoolError> {
        // Try normal allocation first
        {
            let vms = self.vms.read().await;
            if vms.len() < self.config.max_vms {
                drop(vms);
                let vm_id = self.allocate(image, config).await?;
                return Ok((vm_id, None));
            }
        }

        // Pool is full — find the lowest-priority VM to evict
        let evict_id = {
            let vms = self.vms.read().await;
            let candidate = vms
                .iter()
                .filter(|(_, entry)| entry.config.priority < config.priority)
                .min_by_key(|(_, entry)| (entry.config.priority, entry.started_at));

            match candidate {
                Some((id, entry)) => {
                    info!(
                        evicting = %id,
                        evict_priority = %entry.config.priority,
                        new_priority = %config.priority,
                        "evicting lower-priority VM to make room"
                    );
                    id.clone()
                }
                None => {
                    return Err(PoolError::Exhausted {
                        available: 0,
                        requested: 1,
                    });
                }
            }
        };

        // Evict the chosen VM
        self.deallocate(&evict_id).await?;

        // Now allocate
        let vm_id = self.allocate(image, config).await?;
        Ok((vm_id, Some(evict_id)))
    }

    pub async fn deallocate(&self, vm_id: &VmId) -> Result<(), PoolError> {
        let entry = {
            let mut vms = self.vms.write().await;
            vms.remove(vm_id)
                .ok_or_else(|| PoolError::VmNotFound(vm_id.clone()))?
        };

        info!(%vm_id, "deallocating VM");

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopping,
            })
            .await;

        if let Err(e) = self.runtime.stop(vm_id).await {
            warn!(%vm_id, error = %e, "failed to stop VM via runtime");
        }

        drop(entry);

        self.events
            .append(EventPayload::VmLifecycle {
                vm_id: vm_id.clone(),
                state: VmState::Stopped,
            })
            .await;
        self.events.cleanup_vm(vm_id).await;
        Ok(())
    }

    pub async fn health_check(&self) {
        let mut timed_out = Vec::new();
        {
            let vms = self.vms.read().await;
            for (vm_id, entry) in vms.iter() {
                let age_ms = now_ms().saturating_sub(entry.started_at);
                let age_s = age_ms / 1000;
                if age_ms >= self.config.vm_timeout * 1000 {
                    warn!(%vm_id, age_s, timeout = self.config.vm_timeout, "VM exceeded timeout");
                    timed_out.push(vm_id.clone());
                }
                debug!(%vm_id, state = ?entry.state, age_s, "health check");
            }
        }
        for vm_id in timed_out {
            if let Err(e) = self.deallocate(&vm_id).await {
                warn!(%vm_id, error = %e, "failed to deallocate timed-out VM");
            }
        }
    }
}

async fn forward_vm_events(
    vm_id: VmId,
    mut event_rx: mpsc::Receiver<VmEvent>,
    events: Arc<EventLog>,
) {
    while let Some(event) = event_rx.recv().await {
        events
            .append(EventPayload::VmProtocol {
                vm_id: vm_id.clone(),
                event,
            })
            .await;
    }
    debug!(%vm_id, "VM event forwarder stopped");
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolStatus {
    pub total: usize,
    pub allocated: usize,
    pub available: usize,
}

fn generate_vm_id() -> VmId {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros() as u64;
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    VmId::new(format!("vm-{:x}-{:x}", ts, count))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pool(max_vms: usize) -> Arc<Pool<NoRuntime>> {
        let events = EventLog::new();
        Pool::new(
            PoolConfig {
                max_vms,
                health_check_interval: 30,
                vm_timeout: 7200,
            },
            events,
        )
    }

    /// Build the supervisor and return its path.
    async fn build_supervisor() -> std::path::PathBuf {
        let output = tokio::process::Command::new("cargo")
            .args(["build", "-p", "vm-pool-supervisor", "--message-format=json"])
            .output()
            .await
            .unwrap();
        assert!(output.status.success(), "supervisor build failed");

        let stdout = String::from_utf8_lossy(&output.stdout);
        stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|msg| {
                if msg.get("reason")?.as_str()? == "compiler-artifact"
                    && msg.get("target")?.get("name")?.as_str()? == "supervisor"
                {
                    Some(std::path::PathBuf::from(
                        msg.get("executable")?.as_str()?,
                    ))
                } else {
                    None
                }
            })
            .expect("could not find supervisor binary path")
    }

    #[tokio::test]
    async fn allocate_and_status() {
        let pool = test_pool(3);
        assert_eq!(pool.status().await.available, 3);

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert!(!vm_id.as_str().is_empty());
        assert_eq!(pool.status().await.allocated, 1);
    }

    #[tokio::test]
    async fn allocate_until_exhausted() {
        let pool = test_pool(2);
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let result = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await;
        assert!(matches!(result, Err(PoolError::Exhausted { .. })));
    }

    #[tokio::test]
    async fn deallocate_frees_slot() {
        let pool = test_pool(1);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.available, 0);
        pool.deallocate(&vm_id).await.unwrap();
        assert_eq!(pool.status().await.available, 1);
    }

    #[tokio::test]
    async fn deallocate_not_found() {
        let pool = test_pool(3);
        assert!(matches!(
            pool.deallocate(&VmId::new("vm-nope")).await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    #[tokio::test]
    async fn get_vm_state() {
        let pool = test_pool(3);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.get(&vm_id).await, Some(VmState::Ready));
        assert_eq!(pool.get(&VmId::new("vm-missing")).await, None);
    }

    #[tokio::test]
    async fn list_vms() {
        let pool = test_pool(3);
        let vm1 = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        let vm2 = pool
            .allocate(ImageRef::new("auto", "v1"), VmConfig::default())
            .await
            .unwrap();
        let list = pool.list().await;
        assert_eq!(list.len(), 2);
        let ids: Vec<&VmId> = list.iter().map(|(id, _)| id).collect();
        assert!(ids.contains(&&vm1));
        assert!(ids.contains(&&vm2));
    }

    #[tokio::test]
    async fn health_check_removes_timed_out() {
        let events = EventLog::new();
        let pool = Pool::new(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 1,
                vm_timeout: 0,
            },
            events,
        );
        pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert_eq!(pool.status().await.allocated, 1);
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        pool.health_check().await;
        assert_eq!(pool.status().await.allocated, 0);
    }

    #[tokio::test]
    async fn unique_vm_ids() {
        let pool = test_pool(10);
        let mut ids = Vec::new();
        for _ in 0..10 {
            ids.push(
                pool.allocate(ImageRef::new("agent", "v1"), VmConfig::default())
                    .await
                    .unwrap(),
            );
        }
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), 10);
    }

    #[tokio::test]
    async fn send_command_no_runtime() {
        let pool = test_pool(3);
        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();
        assert!(matches!(
            pool.send_command(&vm_id, VmCommand::Ping).await,
            Err(PoolError::VmNotReady(_))
        ));
    }

    #[tokio::test]
    async fn send_command_not_found() {
        let pool = test_pool(3);
        assert!(matches!(
            pool.send_command(&VmId::new("vm-nope"), VmCommand::Ping)
                .await,
            Err(PoolError::VmNotFound(_))
        ));
    }

    // Integration tests using real supervisor process

    #[tokio::test]
    async fn supervisor_runtime_allocate_and_send() {
        let binary = build_supervisor().await;
        let events = EventLog::new();
        let runtime = SupervisorRuntime::new(&binary);
        let pool = Pool::with_runtime(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            events,
            runtime,
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        // Send a ping — it goes to the real supervisor
        pool.send_command(&vm_id, VmCommand::Ping).await.unwrap();

        // Give the event forwarder a moment
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        pool.deallocate(&vm_id).await.unwrap();
        assert_eq!(pool.status().await.allocated, 0);
    }

    #[tokio::test]
    async fn supervisor_runtime_events_forwarded_to_log() {
        let binary = build_supervisor().await;
        let events = EventLog::new();
        let pool = Pool::with_runtime(
            PoolConfig {
                max_vms: 3,
                health_check_interval: 300,
                vm_timeout: 7200,
            },
            events.clone(),
            SupervisorRuntime::new(&binary),
        );

        let vm_id = pool
            .allocate(ImageRef::new("agent", "v1"), VmConfig::default())
            .await
            .unwrap();

        // Execute a command — the supervisor will emit Output + CommandCompleted
        pool.send_command(
            &vm_id,
            VmCommand::Execute {
                command: "echo pool-test".into(),
            },
        )
        .await
        .unwrap();

        // Wait for events to propagate
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let vm_events = events.for_vm(&vm_id).await;
        let protocol_events: Vec<_> = vm_events
            .iter()
            .filter(|e| matches!(e.payload, EventPayload::VmProtocol { .. }))
            .collect();
        assert!(
            !protocol_events.is_empty(),
            "expected VM protocol events in log, got {} total events",
            vm_events.len()
        );

        pool.deallocate(&vm_id).await.unwrap();
    }
}
