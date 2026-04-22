use spawn_lnd::{BitcoinCore, BitcoinCoreConfig, DockerClient};
use uuid::Uuid;

#[tokio::test]
async fn bitcoind_smoke_spawns_and_answers_rpc() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping Bitcoin Core smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    eprintln!("bitcoind smoke: connecting to Docker");
    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("bitcoind-smoke-{}", Uuid::new_v4());
    eprintln!("bitcoind smoke: using cluster_id={cluster_id}");

    let result = async {
        eprintln!("bitcoind smoke: spawning Bitcoin Core");
        let bitcoind =
            BitcoinCore::spawn(&docker, BitcoinCoreConfig::new(cluster_id.clone(), 0)).await?;
        eprintln!(
            "bitcoind smoke: spawned container id={} rpc={} p2p={}",
            bitcoind.container.id, bitcoind.rpc_socket, bitcoind.p2p_socket
        );

        eprintln!("bitcoind smoke: querying getblockchaininfo");
        let info = bitcoind.rpc.get_blockchain_info().await?;
        eprintln!(
            "bitcoind smoke: chain={} blocks={} bestblockhash={}",
            info.chain, info.blocks, info.bestblockhash
        );

        Ok::<_, Box<dyn std::error::Error>>(info)
    }
    .await;
    eprintln!("bitcoind smoke: cleaning up cluster_id={cluster_id}");
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    let info = result.expect("spawn Bitcoin Core and query RPC");

    assert_eq!(info.chain, "regtest");

    let cleanup = cleanup.expect("cleanup Bitcoin Core container");
    eprintln!(
        "bitcoind smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(cleanup.removed >= 1, "expected Bitcoin Core cleanup");
}
