use std::collections::HashMap;

use lnd_grpc_rust::{LndConnectError, LndNodeClients, LndNodeConfig};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    BITCOIND_P2P_PORT, BitcoinCore, BitcoinCoreConfig, BitcoinCoreError, BitcoinRpcError,
    CleanupReport, ConfigError, DockerClient, DockerError, LndConfig, LndDaemon, LndError,
    NodeConfig, SpawnLndConfig,
};

#[derive(Debug)]
pub struct SpawnedCluster {
    docker: DockerClient,
    config: SpawnLndConfig,
    cluster_id: String,
    bitcoinds: Vec<BitcoinCore>,
    nodes: HashMap<String, SpawnedNode>,
    node_order: Vec<String>,
    shutdown: bool,
}

impl SpawnedCluster {
    pub async fn spawn(config: SpawnLndConfig) -> Result<Self, SpawnError> {
        let docker = DockerClient::connect().await?;
        Self::spawn_with_docker(docker, config).await
    }

    pub async fn spawn_with_docker(
        docker: DockerClient,
        config: SpawnLndConfig,
    ) -> Result<Self, SpawnError> {
        let cluster_id = new_cluster_id();
        let cleanup_docker = docker.clone();
        let keep_containers = config.keep_containers;

        match spawn_inner(docker, config, cluster_id.clone()).await {
            Ok(cluster) => Ok(cluster),
            Err(error) => {
                if keep_containers {
                    return Err(error);
                }

                cleanup_docker
                    .cleanup_cluster(&cluster_id)
                    .await
                    .map_err(|source| SpawnError::StartupCleanup {
                        cluster_id,
                        startup_error: error.to_string(),
                        source,
                    })?;
                Err(error)
            }
        }
    }

    pub fn cluster_id(&self) -> &str {
        &self.cluster_id
    }

    pub fn config(&self) -> &SpawnLndConfig {
        &self.config
    }

    pub fn bitcoinds(&self) -> &[BitcoinCore] {
        &self.bitcoinds
    }

    pub fn node(&self, alias: &str) -> Option<&SpawnedNode> {
        self.nodes.get(alias)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &SpawnedNode> {
        self.node_order
            .iter()
            .filter_map(|alias| self.nodes.get(alias))
    }

    pub fn node_aliases(&self) -> impl Iterator<Item = &str> {
        self.node_order.iter().map(String::as_str)
    }

    pub fn node_configs(&self) -> Vec<LndNodeConfig> {
        self.nodes().map(SpawnedNode::node_config).collect()
    }

    pub async fn connect_nodes(&self) -> Result<LndNodeClients, SpawnError> {
        lnd_grpc_rust::connect_nodes(self.node_configs())
            .await
            .map_err(SpawnError::ConnectNodes)
    }

    pub async fn shutdown(&mut self) -> Result<CleanupReport, SpawnError> {
        if self.shutdown || self.config.keep_containers {
            self.shutdown = true;
            return Ok(empty_cleanup_report());
        }

        let report = self.docker.cleanup_cluster(&self.cluster_id).await?;
        self.shutdown = true;
        Ok(report)
    }
}

impl Drop for SpawnedCluster {
    fn drop(&mut self) {
        if !self.shutdown && !self.config.keep_containers {
            eprintln!(
                "spawn-lnd cluster {} dropped without shutdown(); call shutdown().await to remove managed containers",
                self.cluster_id
            );
        }
    }
}

#[derive(Clone, Debug)]
pub struct SpawnedNode {
    alias: String,
    node_index: usize,
    chain_group_index: usize,
    daemon: LndDaemon,
}

impl SpawnedNode {
    fn new(node_index: usize, chain_group_index: usize, daemon: LndDaemon) -> Self {
        Self {
            alias: daemon.alias.clone(),
            node_index,
            chain_group_index,
            daemon,
        }
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn node_index(&self) -> usize {
        self.node_index
    }

    pub fn chain_group_index(&self) -> usize {
        self.chain_group_index
    }

    pub fn lnd(&self) -> &LndDaemon {
        &self.daemon
    }

    pub fn node_config(&self) -> LndNodeConfig {
        self.daemon.node_config()
    }
}

#[derive(Debug, Error)]
pub enum SpawnError {
    #[error(transparent)]
    Config(#[from] ConfigError),

    #[error(transparent)]
    Docker(#[from] DockerError),

    #[error("failed to spawn Bitcoin Core chain group {group_index}")]
    BitcoinCore {
        group_index: usize,
        source: BitcoinCoreError,
    },

    #[error("failed to connect Bitcoin Core chain group {from_group} to group {to_group}")]
    BitcoinPeer {
        from_group: usize,
        to_group: usize,
        source: BitcoinRpcError,
    },

    #[error("Bitcoin Core chain group {group_index} did not expose a bridge IP address")]
    MissingBitcoindIp { group_index: usize },

    #[error("failed to spawn LND node {alias}")]
    Lnd { alias: String, source: LndError },

    #[error(transparent)]
    ConnectNodes(#[from] LndConnectError),

    #[error(
        "startup failed for cluster {cluster_id}, then cleanup failed; startup error: {startup_error}"
    )]
    StartupCleanup {
        cluster_id: String,
        startup_error: String,
        source: DockerError,
    },
}

async fn spawn_inner(
    docker: DockerClient,
    config: SpawnLndConfig,
    cluster_id: String,
) -> Result<SpawnedCluster, SpawnError> {
    let bitcoinds = spawn_bitcoinds(&docker, &config, &cluster_id).await?;
    connect_bitcoind_groups(&bitcoinds).await?;
    let (nodes, node_order) = spawn_lnd_nodes(&docker, &config, &cluster_id, &bitcoinds).await?;
    wait_lnd_nodes_synced(&nodes, &node_order).await?;

    Ok(SpawnedCluster {
        docker,
        config,
        cluster_id,
        bitcoinds,
        nodes,
        node_order,
        shutdown: false,
    })
}

async fn spawn_bitcoinds(
    docker: &DockerClient,
    config: &SpawnLndConfig,
    cluster_id: &str,
) -> Result<Vec<BitcoinCore>, SpawnError> {
    let mut bitcoinds = Vec::with_capacity(config.chain_group_count());

    for group_index in 0..config.chain_group_count() {
        let bitcoind = BitcoinCore::spawn(
            docker,
            BitcoinCoreConfig::new(cluster_id, group_index).image(config.bitcoind_image.clone()),
        )
        .await
        .map_err(|source| SpawnError::BitcoinCore {
            group_index,
            source,
        })?;
        bitcoinds.push(bitcoind);
    }

    Ok(bitcoinds)
}

async fn connect_bitcoind_groups(bitcoinds: &[BitcoinCore]) -> Result<(), SpawnError> {
    for (from_group, from) in bitcoinds.iter().enumerate() {
        for (to_group, to) in bitcoinds.iter().enumerate() {
            if from_group == to_group {
                continue;
            }

            let socket = bitcoind_bridge_socket(to_group, to)?;
            from.rpc
                .add_node(&socket)
                .await
                .map_err(|source| SpawnError::BitcoinPeer {
                    from_group,
                    to_group,
                    source,
                })?;
        }
    }

    Ok(())
}

async fn spawn_lnd_nodes(
    docker: &DockerClient,
    config: &SpawnLndConfig,
    cluster_id: &str,
    bitcoinds: &[BitcoinCore],
) -> Result<(HashMap<String, SpawnedNode>, Vec<String>), SpawnError> {
    let mut nodes = HashMap::with_capacity(config.nodes.len());
    let mut node_order = Vec::with_capacity(config.nodes.len());

    for (node_index, node_config) in config.nodes.iter().enumerate() {
        let chain_group_index = chain_group_index(node_index, config.nodes_per_bitcoind);
        let bitcoind = &bitcoinds[chain_group_index];
        let lnd_config = lnd_config(cluster_id, node_index, node_config, &config.lnd_image);
        let daemon = LndDaemon::spawn(docker, bitcoind, lnd_config)
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: node_config.alias.clone(),
                source,
            })?;
        let node = SpawnedNode::new(node_index, chain_group_index, daemon);

        node_order.push(node.alias.clone());
        nodes.insert(node.alias.clone(), node);
    }

    Ok((nodes, node_order))
}

async fn wait_lnd_nodes_synced(
    nodes: &HashMap<String, SpawnedNode>,
    node_order: &[String],
) -> Result<(), SpawnError> {
    for alias in node_order {
        let node = &nodes[alias];
        node.daemon
            .wait_synced_to_chain()
            .await
            .map_err(|source| SpawnError::Lnd {
                alias: alias.clone(),
                source,
            })?;
    }

    Ok(())
}

fn lnd_config(
    cluster_id: &str,
    node_index: usize,
    node_config: &NodeConfig,
    image: &str,
) -> LndConfig {
    LndConfig::new(cluster_id, node_config.alias.clone(), node_index)
        .image(image)
        .extra_args(node_config.lnd_args.clone())
}

fn chain_group_index(node_index: usize, nodes_per_bitcoind: usize) -> usize {
    node_index / nodes_per_bitcoind
}

fn bitcoind_bridge_socket(
    group_index: usize,
    bitcoind: &BitcoinCore,
) -> Result<String, SpawnError> {
    let ip = bitcoind
        .container
        .ip_address
        .as_deref()
        .ok_or(SpawnError::MissingBitcoindIp { group_index })?;

    Ok(format!("{ip}:{BITCOIND_P2P_PORT}"))
}

fn new_cluster_id() -> String {
    format!("cluster-{}", Uuid::new_v4().simple())
}

fn empty_cleanup_report() -> CleanupReport {
    CleanupReport {
        matched: 0,
        removed: 0,
        failures: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{chain_group_index, lnd_config};
    use crate::NodeConfig;

    #[test]
    fn assigns_nodes_to_chain_groups() {
        let groups = (0..8)
            .map(|node_index| chain_group_index(node_index, 3))
            .collect::<Vec<_>>();

        assert_eq!(groups, [0, 0, 0, 1, 1, 1, 2, 2]);
    }

    #[test]
    fn builds_lnd_config_from_node_config() {
        let node = NodeConfig::new("alice").with_lnd_args(["--alias=Alice", "--color=#3399ff"]);
        let config = lnd_config("cluster-1", 2, &node, "custom/lnd:v1");

        assert_eq!(config.cluster_id, "cluster-1");
        assert_eq!(config.alias, "alice");
        assert_eq!(config.node_index, 2);
        assert_eq!(config.image, "custom/lnd:v1");
        assert_eq!(config.extra_args, ["--alias=Alice", "--color=#3399ff"]);
    }
}
