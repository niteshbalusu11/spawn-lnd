use spawn_lnd::{BitcoinCore, BitcoinCoreConfig, DockerClient, LndConfig, LndDaemon};
use uuid::Uuid;

#[tokio::test]
async fn lnd_smoke_spawns_initializes_and_syncs_to_chain() {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping LND smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return;
    }

    eprintln!("lnd smoke: connecting to Docker");
    let docker = DockerClient::connect().await.expect("connect to Docker");
    let cluster_id = format!("lnd-smoke-{}", Uuid::new_v4());
    eprintln!("lnd smoke: using cluster_id={cluster_id}");

    let result = async {
        eprintln!("lnd smoke: spawning Bitcoin Core");
        let bitcoind =
            BitcoinCore::spawn(&docker, BitcoinCoreConfig::new(cluster_id.clone(), 0)).await?;
        eprintln!(
            "lnd smoke: Bitcoin Core ready id={} rpc={}",
            bitcoind.container.id, bitcoind.rpc_socket
        );

        eprintln!("lnd smoke: spawning and initializing LND alice");
        let lnd = LndDaemon::spawn(
            &docker,
            &bitcoind,
            LndConfig::new(cluster_id.clone(), "alice", 0),
        )
        .await?;
        eprintln!(
            "lnd smoke: LND ready id={} rpc={} p2p={} pubkey={}",
            lnd.container.id, lnd.rpc_socket, lnd.p2p_socket, lnd.public_key
        );

        eprintln!("lnd smoke: connecting gRPC client and querying GetInfo");
        let mut client = lnd.connect().await?;
        let info = client
            .lightning()
            .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
            .await?
            .into_inner();
        eprintln!(
            "lnd smoke: GetInfo synced_to_chain={} block_height={} active_channels={}",
            info.synced_to_chain, info.block_height, info.num_active_channels
        );

        Ok::<_, Box<dyn std::error::Error>>(info)
    }
    .await;
    eprintln!("lnd smoke: cleaning up cluster_id={cluster_id}");
    let cleanup = docker.cleanup_cluster(&cluster_id).await;

    let info = result.expect("spawn initialized synced LND");

    assert!(!info.identity_pubkey.is_empty());
    assert!(info.synced_to_chain, "LND must be synced before ready");

    let cleanup = cleanup.expect("cleanup LND smoke containers");
    eprintln!(
        "lnd smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(cleanup.removed >= 2, "expected bitcoind and LND cleanup");
}
