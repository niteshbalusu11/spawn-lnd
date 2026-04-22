use spawn_lnd::{DEFAULT_CHANNEL_CAPACITY_SAT, SpawnLnd};

#[tokio::test]
async fn channel_smoke_opens_confirms_and_activates_channel()
-> Result<(), Box<dyn std::error::Error>> {
    if std::env::var("RUN_DOCKER_TESTS").as_deref() != Ok("1") {
        eprintln!("skipping channel smoke test; set RUN_DOCKER_TESTS=1 to run it");
        return Ok(());
    }

    let mut cluster = SpawnLnd::builder().nodes(["alice", "bob"]).spawn().await?;

    let result = async {
        let funding = cluster.fund_node("alice").await?;
        let channel = cluster.open_channel("alice", "bob").await?;

        Ok::<_, Box<dyn std::error::Error>>((funding, channel))
    }
    .await;
    let cleanup = cluster.shutdown().await;

    let (funding, channel) = result?;
    let cleanup = cleanup?;
    assert!(
        cleanup.removed >= 3,
        "expected bitcoind and two LND containers to be removed"
    );

    assert_eq!(funding.alias, "alice");
    assert!(funding.spendable_utxo_total_sat >= DEFAULT_CHANNEL_CAPACITY_SAT);

    assert_eq!(channel.from_alias, "alice");
    assert_eq!(channel.to_alias, "bob");
    assert_eq!(
        channel.local_funding_amount_sat,
        DEFAULT_CHANNEL_CAPACITY_SAT
    );
    assert!(!channel.channel_point.is_empty());
    assert!(!channel.confirmation_blocks.is_empty());
    assert!(channel.from_channel.active);
    assert!(channel.to_channel.active);
    assert_eq!(channel.from_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);
    assert_eq!(channel.to_channel.capacity, DEFAULT_CHANNEL_CAPACITY_SAT);

    Ok(())
}
