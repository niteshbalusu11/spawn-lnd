use lnd_grpc_rust::lnrpc::GetInfoRequest;
use spawn_lnd::SpawnLnd;

#[tokio::test]
async fn lifecycle_smoke_stops_starts_and_restarts_nodes() -> Result<(), Box<dyn std::error::Error>>
{
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping lifecycle smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    eprintln!("lifecycle smoke: spawning alice/bob cluster");
    let mut cluster = SpawnLnd::builder().nodes(["alice", "bob"]).spawn().await?;
    eprintln!(
        "lifecycle smoke: cluster_id={} network={} subnet={}",
        cluster.cluster_id(),
        cluster.network().name,
        cluster.network().subnet
    );

    let result = async {
        let bitcoind_ip = cluster.bitcoinds()[0]
            .container
            .ip_address
            .clone()
            .expect("bitcoind static IP");
        let alice_public_key = cluster
            .node("alice")
            .expect("alice")
            .public_key()
            .to_string();
        let bob_public_key = cluster.node("bob").expect("bob").public_key().to_string();

        eprintln!("lifecycle smoke: stopping alice LND");
        cluster.stop_lnd("alice").await?;
        eprintln!("lifecycle smoke: starting alice LND");
        cluster.start_lnd("alice").await?;
        let alice_info = get_info(&cluster, "alice").await?;
        assert_eq!(alice_info.identity_pubkey, alice_public_key);
        assert!(alice_info.synced_to_chain);

        eprintln!("lifecycle smoke: restarting bob LND");
        cluster.restart_lnd("bob").await?;
        let bob_info = get_info(&cluster, "bob").await?;
        assert_eq!(bob_info.identity_pubkey, bob_public_key);
        assert!(bob_info.synced_to_chain);

        eprintln!("lifecycle smoke: stopping bitcoind group 0");
        cluster.stop_bitcoind(0).await?;
        eprintln!("lifecycle smoke: starting bitcoind group 0");
        cluster.start_bitcoind(0).await?;
        assert_eq!(
            cluster.bitcoinds()[0].container.ip_address.as_deref(),
            Some(bitcoind_ip.as_str())
        );
        let chain = cluster.bitcoinds()[0].rpc.get_blockchain_info().await?;
        assert_eq!(chain.chain, "regtest");

        eprintln!("lifecycle smoke: restarting bitcoind group 0");
        cluster.restart_bitcoind(0).await?;
        assert_eq!(
            cluster.bitcoinds()[0].container.ip_address.as_deref(),
            Some(bitcoind_ip.as_str())
        );

        for alias in ["alice", "bob"] {
            let info = get_info(&cluster, alias).await?;
            eprintln!(
                "lifecycle smoke: {alias} pubkey={} synced_to_chain={} block_height={}",
                info.identity_pubkey, info.synced_to_chain, info.block_height
            );
            assert!(info.synced_to_chain);
        }

        Ok::<_, Box<dyn std::error::Error>>(())
    }
    .await;

    eprintln!(
        "lifecycle smoke: shutting down cluster_id={}",
        cluster.cluster_id()
    );
    let cleanup = cluster.shutdown().await;

    result?;
    let cleanup = cleanup?;
    eprintln!(
        "lifecycle smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(
        cleanup.removed >= 4,
        "expected bitcoind, two LND containers, and network to be removed"
    );

    Ok(())
}

async fn get_info(
    cluster: &spawn_lnd::SpawnedCluster,
    alias: &str,
) -> Result<lnd_grpc_rust::lnrpc::GetInfoResponse, Box<dyn std::error::Error>> {
    let mut client = cluster.node(alias).expect("node").lnd().connect().await?;
    let info = client
        .lightning()
        .get_info(GetInfoRequest {})
        .await?
        .into_inner();

    Ok(info)
}
