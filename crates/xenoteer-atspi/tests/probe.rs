//! Portable and real-bus AT-SPI probe tests.

#[cfg(feature = "live-atspi")]
use std::time::Duration;

use xenoteer_atspi::{AtspiProbeError, DisabledAtspiProbe};

#[test]
fn disabled_probe_is_an_explicit_capability_result() {
    assert_eq!(
        DisabledAtspiProbe::probe(),
        Err(AtspiProbeError::FeatureDisabled)
    );
}

#[cfg(feature = "live-atspi")]
#[tokio::test]
#[ignore = "requires session D-Bus, AT-SPI registry, and GTK fixture; run tests/platform/run-atspi-spike.sh"]
async fn live_probe_reaches_registry_and_finds_fixture() -> Result<(), Box<dyn std::error::Error>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    ensure_fixture_process_is_live()?;
    let probe = xenoteer_atspi::LiveAtspiProbe::connect(deadline).await?;

    loop {
        ensure_fixture_process_is_live()?;
        let found_fixture = match probe.inspect_registry(deadline).await {
            Ok(report) => report
                .application_names
                .iter()
                .any(|name| name == "xenoteer-atspi-fixture"),
            Err(error @ AtspiProbeError::Timeout { .. }) => return Err(error.into()),
            Err(_) => false,
        };
        if found_fixture {
            return Ok(());
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(AtspiProbeError::Timeout {
                operation: "fixture registry discovery",
            }
            .into());
        }
        let next_retry = std::cmp::min(now + Duration::from_millis(50), deadline);
        tokio::time::sleep_until(next_retry).await;
        if next_retry == deadline {
            return Err(AtspiProbeError::Timeout {
                operation: "fixture registry discovery",
            }
            .into());
        }
    }
}

#[cfg(feature = "live-atspi")]
fn ensure_fixture_process_is_live() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(pid) = std::env::var("XENOTEER_ATSPI_FIXTURE_PID") else {
        return Ok(());
    };
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map_err(|error| format!("GTK fixture process {pid} disappeared: {error}"))?;
    let state = stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.chars().next())
        .ok_or_else(|| format!("cannot parse process state for GTK fixture {pid}"))?;
    if state == 'Z' {
        return Err(format!("GTK fixture process {pid} exited and is a zombie").into());
    }
    Ok(())
}
