use spawn_lnd::SpawnLnd;

#[tokio::test]
async fn cluster_smoke_spawns_two_nodes_connects_and_cleans()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping cluster smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    let mut cluster = SpawnLnd::builder().nodes(["alice", "bob"]).spawn().await?;

    assert_eq!(cluster.bitcoinds().len(), 1);
    assert_eq!(cluster.node_aliases().collect::<Vec<_>>(), ["alice", "bob"]);
    assert_eq!(cluster.node("alice").expect("alice").chain_group_index(), 0);
    assert_eq!(cluster.node("bob").expect("bob").chain_group_index(), 0);

    let result = async {
        let peer = cluster.connect_peer("alice", "bob").await?;
        let funding = cluster.fund_node("alice").await?;
        let mut clients = cluster.connect_nodes().await?;
        let mut infos = Vec::new();

        for alias in ["alice", "bob"] {
            let info = clients
                .get_mut(alias)
                .expect("client")
                .lightning()
                .get_info(lnd_grpc_rust::lnrpc::GetInfoRequest {})
                .await?
                .into_inner();

            infos.push((
                alias.to_string(),
                info.identity_pubkey,
                info.synced_to_chain,
            ));
        }

        Ok::<_, Box<dyn std::error::Error>>((peer, funding, infos))
    }
    .await;
    let cleanup = cluster.shutdown().await;

    let (peer, funding, infos) = result?;
    let cleanup = cleanup?;
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
    assert!(funding.block_hashes.len() >= 102);
    assert!(funding.confirmed_balance_sat > 0);
    assert!(funding.spendable_utxo_count > 0);
    assert!(funding.spendable_utxo_total_sat > 0);

    for (alias, public_key, synced_to_chain) in infos {
        assert!(!public_key.is_empty());
        assert!(synced_to_chain, "{alias} must be synced before ready");
    }

    Ok(())
}
