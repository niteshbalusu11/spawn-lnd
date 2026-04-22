use spawn_lnd::{BitcoinCore, BitcoinCoreConfig, DockerClient};
use uuid::Uuid;

#[tokio::test]
async fn bitcoind_smoke_spawns_and_answers_rpc() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping Bitcoin Core smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("bitcoind-smoke-{}", Uuid::new_v4());

    let result = BitcoinCore::spawn(&docker, BitcoinCoreConfig::new(cluster_id.clone(), 0)).await;
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    let bitcoind = result.expect("spawn Bitcoin Core");
    let info = bitcoind
        .rpc
        .get_blockchain_info()
        .await
        .expect("get blockchain info");

    assert_eq!(info.chain, "regtest");

    let cleanup = cleanup.expect("cleanup Bitcoin Core container");
    assert!(cleanup.removed >= 1, "expected Bitcoin Core cleanup");
}
