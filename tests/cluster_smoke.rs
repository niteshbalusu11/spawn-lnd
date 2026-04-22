use spawn_lnd::SpawnLnd;

#[tokio::test]
async fn cluster_smoke_spawns_two_nodes_connects_and_cleans()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping cluster smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    eprintln!("cluster smoke: spawning alice/bob cluster");
    let mut cluster = SpawnLnd::builder().nodes(["alice", "bob"]).spawn().await?;
    eprintln!(
        "cluster smoke: cluster_id={} bitcoinds={} aliases={:?}",
        cluster.cluster_id(),
        cluster.bitcoinds().len(),
        cluster.node_aliases().collect::<Vec<_>>()
    );

    assert_eq!(cluster.bitcoinds().len(), 1);
    assert_eq!(cluster.node_aliases().collect::<Vec<_>>(), ["alice", "bob"]);
    assert_eq!(cluster.node("alice").expect("alice").chain_group_index(), 0);
    assert_eq!(cluster.node("bob").expect("bob").chain_group_index(), 0);

    let result = async {
        eprintln!("cluster smoke: connecting alice -> bob");
        let peer = cluster.connect_peer("alice", "bob").await?;
        eprintln!(
            "cluster smoke: peer connected {} -> {} socket={} status={}",
            peer.from_alias, peer.to_alias, peer.socket, peer.status
        );

        eprintln!("cluster smoke: funding alice");
        let funding = cluster.fund_node("alice").await?;
        eprintln!(
            "cluster smoke: funded alice txid={} address={} confirmed_balance_sat={} spendable_utxo_total_sat={}",
            funding.txid,
            funding.address,
            funding.confirmed_balance_sat,
            funding.spendable_utxo_total_sat
        );

        eprintln!("cluster smoke: connecting raw gRPC clients");
        let mut clients = cluster.connect_nodes().await?;
        let mut infos = Vec::new();

        for alias in ["alice", "bob"] {
            eprintln!("cluster smoke: querying GetInfo for {alias}");
            let info = clients
                .get_mut(alias)
                .expect("client")
                .lightning()
                .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
                .await?
                .into_inner();
            eprintln!(
                "cluster smoke: {alias} pubkey={} synced_to_chain={} block_height={}",
                info.identity_pubkey, info.synced_to_chain, info.block_height
            );

            infos.push((
                alias.to_string(),
                info.identity_pubkey,
                info.synced_to_chain,
            ));
        }

        Ok::<_, Box<dyn std::error::Error>>((peer, funding, infos))
    }
    .await;
    eprintln!(
        "cluster smoke: shutting down cluster_id={}",
        cluster.cluster_id()
    );
    let cleanup = cluster.shutdown().await;

    let (peer, funding, infos) = result?;
    let cleanup = cleanup?;
    eprintln!(
        "cluster smoke: cleanup matched={} removed={} failures={}",
        cleanup.matched,
        cleanup.removed,
        cleanup.failures.len()
    );
    assert!(
        cleanup.removed >= 3,
        "expected bitcoind and two LND containers to be removed"
    );

    assert_eq!(peer.from_alias, "alice");
    assert_eq!(peer.to_alias, "bob");
    assert!(!peer.public_key.is_empty());
    assert!(peer.socket.ends_with(":9735"));

    assert_eq!(funding.alias, "alice");
    assert!(!funding.address.is_empty());
    assert!(!funding.txid.is_empty());
    assert_eq!(funding.confirmation_blocks.len(), 1);
    assert!(funding.confirmed_balance_sat > 0);
    assert!(funding.spendable_utxo_count > 0);
    assert!(funding.spendable_utxo_total_sat > 0);

    for (alias, public_key, synced_to_chain) in infos {
        assert!(!public_key.is_empty());
        assert!(synced_to_chain, "{alias} must be synced before ready");
    }

    Ok(())
}
