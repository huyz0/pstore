//! Register after the currently queued qualification build finishes.
#![allow(clippy::expect_used, reason = "fixture errors are asserted by panic")]
use std::collections::BTreeMap;

#[test]
fn peer_configuration_refusals_do_not_echo_embedded_credentials() {
    let bad = "https://operator:NEVER_LOG_PEER_PASSWORD@127.0.0.1:9443";
    for variables in [
        vec![
            ("PSTORE_PEERS", bad),
            ("PSTORE_PEER_SELF", "https://127.0.0.1:9444"),
        ],
        vec![("PSTORE_PEERS", bad)],
        vec![("PSTORE_PEER_SELF", bad)],
    ] {
        let variables: BTreeMap<_, _> = variables.into_iter().collect();
        let error =
            crate::PeerConfig::from_vars(|key| variables.get(key).map(|value| (*value).to_owned()))
                .expect_err("credential-bearing peers must be refused");
        for text in [format!("{error}"), format!("{error:?}")] {
            assert!(
                !text.contains("NEVER_LOG_PEER_PASSWORD"),
                "peer configuration errors must redact credentials"
            );
            assert!(text.contains("PSTORE_PEER"));
        }
    }
    let variables: BTreeMap<_, _> = [
        ("PSTORE_PEER_SELF", bad),
        ("PSTORE_PEER_GOSSIP_ADDR", "127.0.0.1:7000"),
    ]
    .into_iter()
    .collect();
    let error =
        crate::GossipConfig::from_vars(|key| variables.get(key).map(|value| (*value).to_owned()))
            .expect_err("credential-bearing membership identity must be refused");
    assert!(!format!("{error} {error:?}").contains("NEVER_LOG_PEER_PASSWORD"));
}
