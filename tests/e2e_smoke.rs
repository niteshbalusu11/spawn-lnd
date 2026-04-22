use spawn_lnd::{DEFAULT_CHANNEL_CAPACITY_SAT, SpawnLnd};

#[tokio::test]
async fn e2e_smoke_spawns_two_bitcoinds_and_opens_channel_ring()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping e2e smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    let aliases = ["alice", "bob", "carol", "dave"];
    let ring = [
        ("alice", "bob"),
        ("bob", "carol"),
        ("carol", "dave"),
        ("dave", "alice"),
    ];
    let mut cluster = SpawnLnd::builder().nodes(aliases).spawn().await?;

    assert_eq!(
        cluster.bitcoinds().len(),
        2,
        "four LND nodes should be split across two bitcoinds"
    );
    assert_eq!(cluster.node_aliases().collect::<Vec<_>>(), aliases);
    assert_eq!(cluster.node("alice").expect("alice").chain_group_index(), 0);
    assert_eq!(cluster.node("bob").expect("bob").chain_group_index(), 0);
    assert_eq!(cluster.node("carol").expect("carol").chain_group_index(), 0);
    assert_eq!(cluster.node("dave").expect("dave").chain_group_index(), 1);

    let result = async {
        let fundings = cluster.fund_nodes(aliases).await?;

        let mut channels = Vec::new();
        for (from, to) in ring {
            channels.push(cluster.open_channel(from, to).await?);
        }

        let mut clients = cluster.connect_nodes().await?;
        let mut infos = Vec::new();
        for alias in aliases {
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
                info.num_active_channels,
            ));
        }

        let mut chain_tips = Vec::new();
        for bitcoind in cluster.bitcoinds() {
            let info = bitcoind.rpc.get_blockchain_info().await?;
            chain_tips.push((info.blocks, info.bestblockhash));
        }
        eprintln!("e2e chain tips: {chain_tips:?}");

        Ok::<_, Box<dyn std::error::Error>>((fundings, channels, infos, chain_tips))
    }
    .await;
    let cleanup = cluster.shutdown().await;

    let (fundings, channels, infos, chain_tips) = result?;
    let cleanup = cleanup?;
    assert!(
        cleanup.removed >= 6,
        "expected two bitcoinds and four LND containers to be removed"
    );

    assert_eq!(fundings.len(), aliases.len());
    let funding_txid = fundings[0].txid.clone();
    for funding in fundings {
        assert!(aliases.contains(&funding.alias.as_str()));
        assert_eq!(funding.txid, funding_txid);
        assert!(funding.spendable_utxo_total_sat >= DEFAULT_CHANNEL_CAPACITY_SAT);
    }

    assert_eq!(channels.len(), ring.len());
    for (channel, (from, to)) in channels.iter().zip(ring) {
        assert_eq!(channel.from_alias, from);
        assert_eq!(channel.to_alias, to);
        assert_eq!(
            channel.local_funding_amount_sat,
            DEFAULT_CHANNEL_CAPACITY_SAT
        );
        assert!(!channel.channel_point.is_empty());
        assert!(channel.from_channel.active);
        assert!(channel.to_channel.active);
        assert_eq!(channel.from_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);
        assert_eq!(channel.to_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);
    }

    for (alias, public_key, synced_to_chain, active_channels) in infos {
        assert!(!public_key.is_empty());
        assert!(synced_to_chain, "{alias} must be synced");
        assert_eq!(
            active_channels, 2,
            "{alias} should have two active ring channels"
        );
    }

    assert_eq!(chain_tips.len(), 2);
    assert!(
        chain_tips
            .iter()
            .all(|tip| tip.0 == chain_tips[0].0 && tip.1 == chain_tips[0].1),
        "bitcoinds should end on the same tip: {chain_tips:?}"
    );
    assert!(
        chain_tips[0].0 <= 200,
        "batched funding should keep e2e block height low; tips: {chain_tips:?}"
    );

    Ok(())
}
