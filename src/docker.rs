use bollard::{
    Docker,
    container::LogOutput,
    errors::Error as BollardError,
    models::{ContainerCreateBody, ContainerInspectResponse, HostConfig, PortBinding, PortMap},
    query_parameters::{
        CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        DownloadFromContainerOptionsBuilder, InspectContainerOptions, ListContainersOptionsBuilder,
        LogsOptionsBuilder, RemoveContainerOptionsBuilder, StartContainerOptions,
        StopContainerOptionsBuilder,
    },
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, io::Read};
use thiserror::Error;

pub const LABEL_MANAGED: &str = "spawn-lnd";
pub const LABEL_MANAGED_VALUE: &str = "true";
pub const LABEL_CLUSTER: &str = "spawn-lnd.cluster";
pub const LABEL_NODE: &str = "spawn-lnd.node";
pub const LABEL_ROLE: &str = "spawn-lnd.role";

const STOP_TIMEOUT_SECONDS: i32 = 10;
const LOG_TAIL_LINES: &str = "200";
const LOG_MAX_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub struct DockerClient {
    docker: Docker,
}

impl DockerClient {
    pub async fn connect() -> Result<Self, DockerError> {
        let docker =
            Docker::connect_with_defaults().map_err(|source| DockerError::Connect { source })?;

        docker
            .ping()
            .await
            .map_err(|source| DockerError::Ping { source })?;

        Ok(Self { docker })
    }

    pub fn from_bollard(docker: Docker) -> Self {
        Self { docker }
    }

    pub fn inner(&self) -> &Docker {
        &self.docker
    }

    pub async fn ensure_image(&self, image: &str) -> Result<ImageStatus, DockerError> {
        match self.docker.inspect_image(image).await {
            Ok(_) => return Ok(ImageStatus::AlreadyPresent),
            Err(source) if is_not_found_error(&source) => {}
            Err(source) => {
                return Err(DockerError::InspectImage {
                    image: image.to_string(),
                    source,
                });
            }
        }

        let options = CreateImageOptionsBuilder::new().from_image(image).build();
        let mut stream = self.docker.create_image(Some(options), None, None);

        while let Some(result) = stream.next().await {
            result.map_err(|source| DockerError::PullImage {
                image: image.to_string(),
                source,
            })?;
        }

        Ok(ImageStatus::Pulled)
    }

    pub async fn create_and_start(
        &self,
        spec: ContainerSpec,
    ) -> Result<SpawnedContainer, DockerError> {
        self.ensure_image(&spec.image).await?;

        let options = CreateContainerOptionsBuilder::new()
            .name(&spec.name)
            .build();
        let response = self
            .docker
            .create_container(Some(options), spec.create_body())
            .await
            .map_err(|source| DockerError::CreateContainer {
                name: spec.name.clone(),
                image: spec.image.clone(),
                source,
            })?;

        if let Err(source) = self
            .docker
            .start_container(&response.id, None::<StartContainerOptions>)
            .await
        {
            let _ = self.stop_and_remove_container(&response.id).await;
            return Err(DockerError::StartContainer {
                container_id: response.id,
                source,
            });
        }

        let inspect = self
            .docker
            .inspect_container(&response.id, None::<InspectContainerOptions>)
            .await
            .map_err(|source| DockerError::InspectContainer {
                container_id: response.id.clone(),
                source,
            })?;

        SpawnedContainer::from_inspect(response.id, inspect)
    }

    pub async fn copy_file_from_container(
        &self,
        container_id: &str,
        path: &str,
    ) -> Result<Vec<u8>, DockerError> {
        let options = DownloadFromContainerOptionsBuilder::new()
            .path(path)
            .build();
        let mut stream = self
            .docker
            .download_from_container(container_id, Some(options));
        let mut archive = Vec::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| DockerError::DownloadFromContainer {
                container_id: container_id.to_string(),
                path: path.to_string(),
                source,
            })?;
            archive.extend_from_slice(&chunk);
        }

        extract_first_file_from_tar(&archive).map_err(|message| DockerError::ArchiveRead {
            container_id: container_id.to_string(),
            path: path.to_string(),
            message,
        })
    }

    pub async fn cleanup_cluster(&self, cluster_id: &str) -> Result<CleanupReport, DockerError> {
        self.cleanup_by_labels(cluster_label_filters(cluster_id))
            .await
    }

    pub async fn cleanup_all(&self) -> Result<CleanupReport, DockerError> {
        self.cleanup_by_labels(managed_label_filters()).await
    }

    pub async fn managed_container_ids(&self) -> Result<Vec<String>, DockerError> {
        self.container_ids_by_labels(managed_label_filters()).await
    }

    pub async fn cluster_container_ids(
        &self,
        cluster_id: &str,
    ) -> Result<Vec<String>, DockerError> {
        self.container_ids_by_labels(cluster_label_filters(cluster_id))
            .await
    }

    pub async fn container_logs(&self, container_id: &str) -> Result<String, DockerError> {
        let options = LogsOptionsBuilder::default()
            .stdout(true)
            .stderr(true)
            .tail(LOG_TAIL_LINES)
            .build();
        let mut stream = self.docker.logs(container_id, Some(options));
        let mut logs = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|source| DockerError::ReadContainerLogs {
                container_id: container_id.to_string(),
                source,
            })?;
            append_log_output(&mut logs, chunk);

            if logs.len() > LOG_MAX_BYTES {
                logs.truncate(LOG_MAX_BYTES);
                logs.push_str("\n<truncated>");
                break;
            }
        }

        Ok(logs)
    }

    pub fn rollback_guard(&self) -> StartupRollback<'_> {
        StartupRollback::new(self)
    }

    pub async fn rollback_containers<I>(
        &self,
        container_ids: I,
    ) -> Result<CleanupReport, DockerError>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let mut report = CleanupReport {
            matched: 0,
            removed: 0,
            failures: Vec::new(),
        };

        for container_id in container_ids {
            report.matched += 1;
            let container_id = container_id.into();

            match self.stop_and_remove_container(&container_id).await {
                Ok(()) => report.removed += 1,
                Err(failure) => report.failures.push(failure),
            }
        }

        if report.failures.is_empty() {
            Ok(report)
        } else {
            Err(DockerError::cleanup_failed(report))
        }
    }

    async fn cleanup_by_labels(
        &self,
        label_filters: HashMap<String, Vec<String>>,
    ) -> Result<CleanupReport, DockerError> {
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&label_filters)
            .build();
        let containers = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(|source| DockerError::ListContainers { source })?;

        let mut report = CleanupReport {
            matched: containers.len(),
            removed: 0,
            failures: Vec::new(),
        };

        for container in containers {
            let Some(container_id) = container.id else {
                report.failures.push(CleanupFailure {
                    container_id: "<missing>".to_string(),
                    operation: "inspect".to_string(),
                    message: "container summary did not include an id".to_string(),
                });
                continue;
            };

            match self.stop_and_remove_container(&container_id).await {
                Ok(()) => report.removed += 1,
                Err(failure) => report.failures.push(failure),
            }
        }

        if report.failures.is_empty() {
            Ok(report)
        } else {
            Err(DockerError::cleanup_failed(report))
        }
    }

    async fn container_ids_by_labels(
        &self,
        label_filters: HashMap<String, Vec<String>>,
    ) -> Result<Vec<String>, DockerError> {
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&label_filters)
            .build();
        let containers = self
            .docker
            .list_containers(Some(options))
            .await
            .map_err(|source| DockerError::ListContainers { source })?;

        Ok(containers
            .into_iter()
            .filter_map(|container| container.id)
            .collect())
    }

    async fn stop_and_remove_container(&self, container_id: &str) -> Result<(), CleanupFailure> {
        let stop_options = StopContainerOptionsBuilder::new()
            .t(STOP_TIMEOUT_SECONDS)
            .build();

        if let Err(source) = self
            .docker
            .stop_container(container_id, Some(stop_options))
            .await
        {
            if !is_ignorable_stop_error(&source) {
                return Err(CleanupFailure::from_error(container_id, "stop", source));
            }
        }

        let remove_options = RemoveContainerOptionsBuilder::new()
            .force(true)
            .v(true)
            .build();

        if let Err(source) = self
            .docker
            .remove_container(container_id, Some(remove_options))
            .await
        {
            if !is_not_found_error(&source) {
                return Err(CleanupFailure::from_error(container_id, "remove", source));
            }
        }

        Ok(())
    }
}

#[derive(Debug)]
pub struct StartupRollback<'a> {
    docker: &'a DockerClient,
    container_ids: Vec<String>,
    disarmed: bool,
}

impl<'a> StartupRollback<'a> {
    fn new(docker: &'a DockerClient) -> Self {
        Self {
            docker,
            container_ids: Vec::new(),
            disarmed: false,
        }
    }

    pub fn record(&mut self, container: &SpawnedContainer) {
        self.record_id(container.id.clone());
    }

    pub fn record_id(&mut self, container_id: impl Into<String>) {
        self.container_ids.push(container_id.into());
    }

    pub fn container_ids(&self) -> &[String] {
        &self.container_ids
    }

    pub fn disarm(mut self) -> Vec<String> {
        self.disarmed = true;
        std::mem::take(&mut self.container_ids)
    }

    pub async fn rollback(mut self) -> Result<CleanupReport, DockerError> {
        self.disarmed = true;
        let container_ids = std::mem::take(&mut self.container_ids);
        self.docker.rollback_containers(container_ids).await
    }
}

impl Drop for StartupRollback<'_> {
    fn drop(&mut self) {
        if !self.disarmed && !self.container_ids.is_empty() {
            eprintln!(
                "spawn-lnd startup rollback guard dropped with {} tracked container(s); call rollback().await to clean them up",
                self.container_ids.len()
            );
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ContainerSpec {
    pub name: String,
    pub image: String,
    pub cmd: Vec<String>,
    pub env: Vec<String>,
    pub labels: HashMap<String, String>,
    pub exposed_ports: Vec<u16>,
    pub network: Option<String>,
}

impl ContainerSpec {
    pub fn new(name: impl Into<String>, image: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            image: image.into(),
            cmd: Vec::new(),
            env: Vec::new(),
            labels: HashMap::new(),
            exposed_ports: Vec::new(),
            network: None,
        }
    }

    pub fn cmd<I, S>(mut self, cmd: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.cmd = cmd.into_iter().map(Into::into).collect();
        self
    }

    pub fn env<I, S>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.env = env.into_iter().map(Into::into).collect();
        self
    }

    pub fn labels(mut self, labels: HashMap<String, String>) -> Self {
        self.labels = labels;
        self
    }

    pub fn expose_port(mut self, port: u16) -> Self {
        self.exposed_ports.push(port);
        self
    }

    pub fn expose_ports<I>(mut self, ports: I) -> Self
    where
        I: IntoIterator<Item = u16>,
    {
        self.exposed_ports.extend(ports);
        self
    }

    pub fn network(mut self, network: impl Into<String>) -> Self {
        self.network = Some(network.into());
        self
    }

    fn create_body(&self) -> ContainerCreateBody {
        ContainerCreateBody {
            image: Some(self.image.clone()),
            cmd: (!self.cmd.is_empty()).then(|| self.cmd.clone()),
            env: (!self.env.is_empty()).then(|| self.env.clone()),
            labels: (!self.labels.is_empty()).then(|| self.labels.clone()),
            exposed_ports: (!self.exposed_ports.is_empty())
                .then(|| exposed_ports(&self.exposed_ports)),
            host_config: Some(HostConfig {
                auto_remove: Some(false),
                network_mode: self.network.clone(),
                port_bindings: (!self.exposed_ports.is_empty())
                    .then(|| port_bindings(&self.exposed_ports)),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SpawnedContainer {
    pub id: String,
    pub name: Option<String>,
    pub ip_address: Option<String>,
    pub host_ports: HashMap<u16, u16>,
}

impl SpawnedContainer {
    fn from_inspect(id: String, inspect: ContainerInspectResponse) -> Result<Self, DockerError> {
        let network_settings = inspect.network_settings.as_ref();
        Ok(Self {
            id,
            name: inspect
                .name
                .map(|name| name.trim_start_matches('/').to_string()),
            ip_address: container_ip_address(network_settings),
            host_ports: published_tcp_ports(network_settings)?,
        })
    }

    pub fn host_port(&self, container_port: u16) -> Option<u16> {
        self.host_ports.get(&container_port).copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContainerRole {
    Bitcoind,
    Lnd,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ImageStatus {
    AlreadyPresent,
    Pulled,
}

impl ContainerRole {
    pub fn as_label_value(self) -> &'static str {
        match self {
            Self::Bitcoind => "bitcoind",
            Self::Lnd => "lnd",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CleanupReport {
    pub matched: usize,
    pub removed: usize,
    pub failures: Vec<CleanupFailure>,
}

impl CleanupReport {
    pub fn is_success(&self) -> bool {
        self.failures.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CleanupFailure {
    pub container_id: String,
    pub operation: String,
    pub message: String,
}

impl CleanupFailure {
    fn from_error(container_id: &str, operation: &'static str, source: BollardError) -> Self {
        Self {
            container_id: container_id.to_string(),
            operation: operation.to_string(),
            message: source.to_string(),
        }
    }
}

#[derive(Debug, Error)]
pub enum DockerError {
    #[error("failed to connect to Docker")]
    Connect { source: BollardError },

    #[error("failed to ping Docker")]
    Ping { source: BollardError },

    #[error("failed to list Docker containers")]
    ListContainers { source: BollardError },

    #[error("failed to inspect Docker image {image}")]
    InspectImage { image: String, source: BollardError },

    #[error("failed to pull Docker image {image}")]
    PullImage { image: String, source: BollardError },

    #[error("failed to create Docker container {name} from image {image}")]
    CreateContainer {
        name: String,
        image: String,
        source: BollardError,
    },

    #[error("failed to start Docker container {container_id}")]
    StartContainer {
        container_id: String,
        source: BollardError,
    },

    #[error("failed to inspect Docker container {container_id}")]
    InspectContainer {
        container_id: String,
        source: BollardError,
    },

    #[error("Docker reported invalid host port {host_port} for container port {container_port}")]
    InvalidPublishedPort {
        container_port: u16,
        host_port: String,
    },

    #[error("failed to download {path} from Docker container {container_id}")]
    DownloadFromContainer {
        container_id: String,
        path: String,
        source: BollardError,
    },

    #[error("failed to read archived file {path} from Docker container {container_id}: {message}")]
    ArchiveRead {
        container_id: String,
        path: String,
        message: String,
    },

    #[error("failed to read logs from Docker container {container_id}")]
    ReadContainerLogs {
        container_id: String,
        source: BollardError,
    },

    #[error("failed to clean up {count} Docker container(s)")]
    CleanupFailed {
        #[source]
        report: CleanupReportError,
        count: usize,
    },
}

impl DockerError {
    fn cleanup_failed(report: CleanupReport) -> Self {
        Self::CleanupFailed {
            count: report.failures.len(),
            report: CleanupReportError(report),
        }
    }
}

#[derive(Debug)]
pub struct CleanupReportError(pub CleanupReport);

impl std::fmt::Display for CleanupReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} cleanup failure(s) after matching {} container(s)",
            self.0.failures.len(),
            self.0.matched
        )
    }
}

impl std::error::Error for CleanupReportError {}

pub fn managed_container_labels(
    cluster_id: &str,
    role: ContainerRole,
    node_alias: Option<&str>,
) -> HashMap<String, String> {
    let mut labels = HashMap::from([
        (LABEL_MANAGED.to_string(), LABEL_MANAGED_VALUE.to_string()),
        (LABEL_CLUSTER.to_string(), cluster_id.to_string()),
        (LABEL_ROLE.to_string(), role.as_label_value().to_string()),
    ]);

    if let Some(node_alias) = node_alias {
        labels.insert(LABEL_NODE.to_string(), node_alias.to_string());
    }

    labels
}

pub fn managed_label_filters() -> HashMap<String, Vec<String>> {
    label_filters([format!("{LABEL_MANAGED}={LABEL_MANAGED_VALUE}")])
}

pub fn cluster_label_filters(cluster_id: &str) -> HashMap<String, Vec<String>> {
    label_filters([
        format!("{LABEL_MANAGED}={LABEL_MANAGED_VALUE}"),
        format!("{LABEL_CLUSTER}={cluster_id}"),
    ])
}

fn label_filters(labels: impl IntoIterator<Item = String>) -> HashMap<String, Vec<String>> {
    HashMap::from([("label".to_string(), labels.into_iter().collect())])
}

fn exposed_ports(ports: &[u16]) -> HashMap<String, HashMap<(), ()>> {
    ports
        .iter()
        .copied()
        .map(|port| (tcp_port_key(port), HashMap::new()))
        .collect()
}

fn port_bindings(ports: &[u16]) -> PortMap {
    ports
        .iter()
        .copied()
        .map(|port| {
            (
                tcp_port_key(port),
                Some(vec![PortBinding {
                    host_ip: Some("127.0.0.1".to_string()),
                    host_port: Some(String::new()),
                }]),
            )
        })
        .collect()
}

fn tcp_port_key(port: u16) -> String {
    format!("{port}/tcp")
}

fn container_ip_address(
    network_settings: Option<&bollard::models::NetworkSettings>,
) -> Option<String> {
    network_settings
        .and_then(|settings| settings.ip_address.clone())
        .filter(|ip| !ip.is_empty())
        .or_else(|| {
            network_settings
                .and_then(|settings| settings.networks.as_ref())
                .and_then(|networks| {
                    networks
                        .values()
                        .filter_map(|endpoint| endpoint.ip_address.as_ref())
                        .find(|ip| !ip.is_empty())
                        .cloned()
                })
        })
}

fn published_tcp_ports(
    network_settings: Option<&bollard::models::NetworkSettings>,
) -> Result<HashMap<u16, u16>, DockerError> {
    let Some(ports) = network_settings.and_then(|settings| settings.ports.as_ref()) else {
        return Ok(HashMap::new());
    };

    let mut mapped = HashMap::new();
    for (key, bindings) in ports {
        let Some(container_port) = parse_tcp_port_key(key) else {
            continue;
        };
        let Some(binding) = bindings
            .as_ref()
            .and_then(|bindings| bindings.iter().find(|binding| binding.host_port.is_some()))
        else {
            continue;
        };
        let Some(host_port) = binding.host_port.as_ref() else {
            continue;
        };
        let host_port =
            host_port
                .parse::<u16>()
                .map_err(|_| DockerError::InvalidPublishedPort {
                    container_port,
                    host_port: host_port.clone(),
                })?;

        mapped.insert(container_port, host_port);
    }

    Ok(mapped)
}

fn parse_tcp_port_key(key: &str) -> Option<u16> {
    let (port, protocol) = key.split_once('/')?;
    (protocol == "tcp")
        .then(|| port.parse::<u16>().ok())
        .flatten()
}

fn extract_first_file_from_tar(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive = tar::Archive::new(bytes);
    let entries = archive
        .entries()
        .map_err(|err| format!("failed to read tar entries: {err}"))?;

    for entry in entries {
        let mut entry = entry.map_err(|err| format!("failed to read tar entry: {err}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }

        let mut file = Vec::new();
        entry
            .read_to_end(&mut file)
            .map_err(|err| format!("failed to read tar file contents: {err}"))?;
        return Ok(file);
    }

    Err("archive did not contain a regular file".to_string())
}

fn append_log_output(logs: &mut String, output: LogOutput) {
    let prefix = match &output {
        LogOutput::StdErr { .. } => "stderr",
        LogOutput::StdOut { .. } => "stdout",
        LogOutput::StdIn { .. } => "stdin",
        LogOutput::Console { .. } => "console",
    };
    let message = String::from_utf8_lossy(output.as_ref());

    logs.push('[');
    logs.push_str(prefix);
    logs.push_str("] ");
    logs.push_str(&message);

    if !logs.ends_with('\n') {
        logs.push('\n');
    }
}

fn is_ignorable_stop_error(error: &BollardError) -> bool {
    matches!(docker_status_code(error), Some(304 | 404))
}

fn is_not_found_error(error: &BollardError) -> bool {
    matches!(docker_status_code(error), Some(404))
}

fn docker_status_code(error: &BollardError) -> Option<u16> {
    match error {
        BollardError::DockerResponseServerError { status_code, .. } => Some(*status_code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use bollard::models::{NetworkSettings, PortBinding};
    use std::collections::HashMap;

    use super::{
        ContainerRole, ContainerSpec, LABEL_CLUSTER, LABEL_MANAGED, LABEL_MANAGED_VALUE,
        LABEL_NODE, LABEL_ROLE, append_log_output, cluster_label_filters,
        extract_first_file_from_tar, managed_container_labels, managed_label_filters,
        parse_tcp_port_key, published_tcp_ports,
    };

    #[test]
    fn builds_managed_labels_for_lnd_node() {
        let labels = managed_container_labels("cluster-1", ContainerRole::Lnd, Some("alice"));

        assert_eq!(labels.get(LABEL_MANAGED).unwrap(), LABEL_MANAGED_VALUE);
        assert_eq!(labels.get(LABEL_CLUSTER).unwrap(), "cluster-1");
        assert_eq!(labels.get(LABEL_ROLE).unwrap(), "lnd");
        assert_eq!(labels.get(LABEL_NODE).unwrap(), "alice");
    }

    #[test]
    fn builds_managed_labels_for_bitcoind_group() {
        let labels = managed_container_labels("cluster-1", ContainerRole::Bitcoind, None);

        assert_eq!(labels.get(LABEL_MANAGED).unwrap(), LABEL_MANAGED_VALUE);
        assert_eq!(labels.get(LABEL_CLUSTER).unwrap(), "cluster-1");
        assert_eq!(labels.get(LABEL_ROLE).unwrap(), "bitcoind");
        assert!(!labels.contains_key(LABEL_NODE));
    }

    #[test]
    fn builds_cleanup_all_filter() {
        let filters = managed_label_filters();

        assert_eq!(
            filters.get("label").unwrap(),
            &vec!["spawn-lnd=true".to_string()]
        );
    }

    #[test]
    fn builds_cleanup_cluster_filter() {
        let filters = cluster_label_filters("cluster-1");

        assert_eq!(
            filters.get("label").unwrap(),
            &vec![
                "spawn-lnd=true".to_string(),
                "spawn-lnd.cluster=cluster-1".to_string()
            ]
        );
    }

    #[test]
    fn builds_container_create_body_with_labels_and_ports() {
        let labels = managed_container_labels("cluster-1", ContainerRole::Bitcoind, None);
        let spec = ContainerSpec::new("spawn-lnd-test", "lightninglabs/bitcoin-core:30")
            .cmd(["bitcoind", "-regtest"])
            .env(["A=B"])
            .labels(labels)
            .expose_ports([18443, 18444])
            .network("bridge");

        let body = spec.create_body();
        let host_config = body.host_config.expect("host config");
        let port_bindings = host_config.port_bindings.expect("port bindings");

        assert_eq!(body.image.as_deref(), Some("lightninglabs/bitcoin-core:30"));
        assert_eq!(body.cmd.unwrap(), ["bitcoind", "-regtest"]);
        assert_eq!(body.env.unwrap(), ["A=B"]);
        assert_eq!(
            body.labels.unwrap().get(LABEL_MANAGED).unwrap(),
            LABEL_MANAGED_VALUE
        );
        assert_eq!(host_config.auto_remove, Some(false));
        assert_eq!(host_config.network_mode.as_deref(), Some("bridge"));
        assert!(body.exposed_ports.unwrap().contains_key("18443/tcp"));

        let binding = port_bindings
            .get("18443/tcp")
            .and_then(|bindings| bindings.as_ref())
            .and_then(|bindings| bindings.first())
            .expect("port binding");
        assert_eq!(binding.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(binding.host_port.as_deref(), Some(""));
    }

    #[test]
    fn parses_tcp_port_keys() {
        assert_eq!(parse_tcp_port_key("10009/tcp"), Some(10009));
        assert_eq!(parse_tcp_port_key("10009/udp"), None);
        assert_eq!(parse_tcp_port_key("not-a-port/tcp"), None);
    }

    #[test]
    fn extracts_published_tcp_ports() {
        let settings = NetworkSettings {
            ports: Some(HashMap::from([
                (
                    "10009/tcp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some("49153".to_string()),
                    }]),
                ),
                (
                    "9735/udp".to_string(),
                    Some(vec![PortBinding {
                        host_ip: Some("127.0.0.1".to_string()),
                        host_port: Some("49154".to_string()),
                    }]),
                ),
            ])),
            ..Default::default()
        };

        let ports = published_tcp_ports(Some(&settings)).expect("published ports");

        assert_eq!(ports.get(&10009), Some(&49153));
        assert!(!ports.contains_key(&9735));
    }

    #[test]
    fn extracts_first_regular_file_from_tar() {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            let content = b"certificate-bytes";
            let mut header = tar::Header::new_gnu();
            header.set_path("tls.cert").expect("path");
            header.set_size(content.len() as u64);
            header.set_cksum();
            builder
                .append(&header, &content[..])
                .expect("append tar entry");
            builder.finish().expect("finish tar");
        }

        let file = extract_first_file_from_tar(&archive).expect("file contents");

        assert_eq!(file, b"certificate-bytes");
    }

    #[test]
    fn errors_when_tar_has_no_regular_file() {
        let mut archive = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut archive);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Directory);
            header.set_path("empty-dir").expect("path");
            header.set_size(0);
            header.set_cksum();
            builder
                .append(&header, std::io::empty())
                .expect("append directory");
            builder.finish().expect("finish tar");
        }

        let error = extract_first_file_from_tar(&archive).expect_err("no file");

        assert_eq!(error, "archive did not contain a regular file");
    }

    #[test]
    fn rollback_guard_tracks_and_disarms_ids() {
        let docker = super::DockerClient::from_bollard(
            bollard::Docker::connect_with_http(
                "http://127.0.0.1:65535",
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .expect("construct Docker client"),
        );
        let mut rollback = docker.rollback_guard();

        rollback.record_id("container-a");
        rollback.record_id("container-b");

        assert_eq!(rollback.container_ids(), ["container-a", "container-b"]);
        assert_eq!(rollback.disarm(), ["container-a", "container-b"]);
    }

    #[test]
    fn formats_log_output_with_stream_prefix() {
        let mut logs = String::new();

        append_log_output(
            &mut logs,
            bollard::container::LogOutput::StdErr {
                message: "failure".into(),
            },
        );

        assert_eq!(logs, "[stderr] failure\n");
    }
}
