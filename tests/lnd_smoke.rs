use spawn_lnd::{BitcoinCore, BitcoinCoreConfig, DockerClient, LndConfig, LndDaemon};
use uuid::Uuid;

#[tokio::test]
async fn lnd_smoke_spawns_initializes_and_syncs_to_chain() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping LND smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("lnd-smoke-{}", Uuid::new_v4());

    let result = async {
        let bitcoind =
            BitcoinCore::spawn(&docker, BitcoinCoreConfig::new(cluster_id.clone(), 0)).await?;
        let lnd = LndDaemon::spawn(
            &docker,
            &bitcoind,
            LndConfig::new(cluster_id.clone(), "alice", 0),
        )
        .await?;
        let mut client = lnd.connect().await?;
        let info = client
            .lightning()
            .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
            .await?
            .into_inner();

        Ok::<_, Box<dyn std::error::Error>>(info)
    }
    .await;
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    let info = result.expect("spawn initialized synced LND");

    assert!(!info.identity_pubkey.is_empty());
    assert!(info.synced_to_chain, "LND must be synced before ready");

    let cleanup = cleanup.expect("cleanup LND smoke containers");
    assert!(cleanup.removed >= 2, "expected bitcoind and LND cleanup");
}
