//! Wire-shape and client-boundary tests for PID correlation.

use super::*;
use crate::MAX_PROCESS_CORRELATION_PIDS;

#[test]
fn correlation_pid_wire_collection_is_strict_unique_and_bounded()
-> Result<(), Box<dyn std::error::Error>> {
    let generation = DesktopGeneration::new();
    let valid = serde_json::json!({
        "operation": "correlate_pids",
        "desktop_generation": generation,
        "pids": [10, 11]
    });
    let request: BrokerRequest = serde_json::from_value(valid.clone())?;
    let BrokerRequest::CorrelatePids {
        desktop_generation,
        pids,
    } = request
    else {
        return Err("wrong request variant".into());
    };
    assert_eq!(desktop_generation, generation);
    assert_eq!(pids.as_slice(), &[10, 11]);

    for invalid in [
        serde_json::json!({
            "operation": "correlate_pids",
            "desktop_generation": generation,
            "pids": []
        }),
        serde_json::json!({
            "operation": "correlate_pids",
            "desktop_generation": generation,
            "pids": [0]
        }),
        serde_json::json!({
            "operation": "correlate_pids",
            "desktop_generation": generation,
            "pids": [10, 10]
        }),
    ] {
        assert!(serde_json::from_value::<BrokerRequest>(invalid).is_err());
    }
    let mut excess = valid;
    excess["pids"] = serde_json::Value::Array(
        (1..=u32::try_from(MAX_PROCESS_CORRELATION_PIDS + 1)?)
            .map(serde_json::Value::from)
            .collect(),
    );
    assert!(serde_json::from_value::<BrokerRequest>(excess).is_err());
    Ok(())
}

#[tokio::test]
async fn client_rejects_invalid_batch_before_opening_a_socket() {
    let client = BrokerClient::new("/path/that/must/not/exist");
    let generation = DesktopGeneration::new();
    for pids in [Vec::new(), vec![0], vec![10, 10]] {
        assert!(matches!(
            client.correlate_pids(generation, pids).await,
            Err(BrokerClientError::Rejected {
                code: BrokerErrorCode::InvalidRequest
            })
        ));
    }
}

#[test]
fn correlation_reply_round_trips_typed_evidence_without_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let process = ProtocolProcessRef {
        desktop_generation: DesktopGeneration::new(),
        pid: 100,
        proc_start_ticks: 1_000,
        launch_id: LaunchId::new(),
    };
    let reply = BrokerReply::PidCorrelations {
        entries: vec![
            BrokerPidCorrelation {
                pid: 100,
                evidence: BrokerPidCorrelationEvidence::ManagedLeader { process },
            },
            BrokerPidCorrelation {
                pid: 101,
                evidence: BrokerPidCorrelationEvidence::ManagedProcessGroup { process },
            },
            BrokerPidCorrelation {
                pid: 200,
                evidence: BrokerPidCorrelationEvidence::NoMatch,
            },
        ],
    };
    let encoded = serde_json::to_value(&reply)?;
    assert!(encoded.get("principal").is_none());
    assert!(encoded.get("grant").is_none());
    assert_eq!(serde_json::from_value::<BrokerReply>(encoded)?, reply);
    Ok(())
}
