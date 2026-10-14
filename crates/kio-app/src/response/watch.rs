use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum WatchResponse {
    Foreground(WatchLifecycleResponse),
    Status(WatchLifecycleResponse),
    Stop(WatchLifecycleResponse),
    ServiceInstalled(WatchServiceInstalledResponse),
    ServiceLifecycle(WatchServiceLifecycleResponse),
    ServiceStatus(WatchServiceStatusResponse),
    ServiceRunner(WatchServiceRunnerResponse),
}
#[derive(Debug, Serialize)]
pub struct WatchLifecycleResponse {
    pub status: String,
    pub root: PathBuf,
    pub last_observation: Option<WatchObservation>,
}
// Move InstanceState here and import WatchObservation as InstanceState in the
// existing producer. Strict persisted schema and nullable fields stay identical.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchObservation {
    pub version: u8,
    pub instance: String,
    pub root: PathBuf,
    pub scope_id: String,
    pub pid: u32,
    pub backend: String,
    pub backlog: usize,
    pub degraded: bool,
    pub last_success_ms: Option<u64>,
    pub last_failure: Option<String>,
    pub observed_at_ms: u64,
    pub reconcile_interval_seconds: u64,
}
#[derive(Debug, Serialize)]
pub struct WatchServiceInstalledResponse {
    pub status: String,
    pub service_id: String,
    pub root: PathBuf,
    pub native_registration: String,
}
#[derive(Debug, Serialize)]
pub struct WatchServiceLifecycleResponse {
    pub status: String,
    pub service_id: String,
    pub root: PathBuf,
}
#[derive(Debug, Serialize)]
pub struct WatchServiceStatusResponse {
    pub status: String,
    pub service_id: String,
    pub root: PathBuf,
    pub install_phase: ServicePhase,
    pub native_registration: NativeServiceState,
    pub instance: WatchLifecycleResponse,
}
// Move the existing ServicePhase enum here; reimport it into watch_service.rs.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServicePhase {
    Installing,
    Installed,
}
#[derive(Debug, Serialize)]
pub struct NativeServiceState {
    pub registered: bool,
    pub active: bool,
}
#[derive(Debug, Serialize)]
pub struct WatchServiceRunnerResponse {
    pub status: String,
    pub service_id: String,
}
