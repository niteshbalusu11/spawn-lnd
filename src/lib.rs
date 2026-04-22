//! Test utilities for spawning Docker-backed LND and Bitcoin Core regtest
//! clusters.
//!
//! The crate is library-first: integration tests should use the public API from
//! here, while binaries and examples stay thin wrappers over the library.

mod bitcoin;
mod cluster;
mod config;
mod docker;
mod lnd;

pub use bitcoin::{
    BITCOIND_P2P_PORT, BITCOIND_RPC_PORT, BitcoinCore, BitcoinCoreConfig, BitcoinCoreError,
    BitcoinRpcAuth, BitcoinRpcClient, BitcoinRpcError, BlockInfo, BlockchainInfo,
    DEFAULT_BITCOIN_RPC_USER, bitcoin_core_auth_hmac, bitcoin_core_rpcauth,
};
pub use cluster::{
    DEFAULT_FUNDING_MATURITY_BLOCKS, FundingReport, PeerConnection, SpawnError, SpawnedCluster,
    SpawnedNode,
};
pub use config::{
    ConfigError, DEFAULT_BITCOIND_IMAGE, DEFAULT_LND_IMAGE, DEFAULT_NODE_ALIAS,
    DEFAULT_NODES_PER_BITCOIND, ENV_BITCOIND_IMAGE, ENV_KEEP_CONTAINERS, ENV_LND_IMAGE,
    ENV_NODES_PER_BITCOIND, NodeConfig, SpawnLnd, SpawnLndBuilder, SpawnLndConfig,
};
pub use docker::{
    CleanupFailure, CleanupReport, ContainerRole, ContainerSpec, DockerClient, DockerError,
    ImageStatus, LABEL_CLUSTER, LABEL_MANAGED, LABEL_MANAGED_VALUE, LABEL_NODE, LABEL_ROLE,
    SpawnedContainer, StartupRollback, cluster_label_filters, managed_container_labels,
    managed_label_filters,
};
pub use lnd::{
    DEFAULT_GENERATE_ADDRESS, LND_ADMIN_MACAROON_PATH, LND_GRPC_PORT, LND_P2P_PORT,
    LND_TLS_CERT_PATH, LND_WALLET_PASSWORD, LndConfig, LndDaemon, LndError,
};

/// Current crate version as declared by Cargo.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::VERSION;

    #[test]
    fn exposes_crate_version() {
        assert!(!VERSION.is_empty());
    }
}
