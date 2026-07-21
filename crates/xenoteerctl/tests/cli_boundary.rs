//! Process-level checks for the Phase-0 command-line package boundary.

use std::process::Command;

fn xenoteerctl() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xenoteerctl"))
}

#[test]
fn version_identifies_the_exact_binary() -> Result<(), Box<dyn std::error::Error>> {
    let output = xenoteerctl().arg("--version").output()?;
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout)?.starts_with("xenoteerctl 0.1.0"));
    Ok(())
}

#[test]
fn phase_zero_commands_do_not_pretend_to_contact_a_daemon() -> Result<(), Box<dyn std::error::Error>>
{
    for command in ["status", "doctor"] {
        let output = xenoteerctl().arg(command).output()?;
        assert_eq!(output.status.code(), Some(7));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr)?;
        assert!(stderr.contains("not yet wired in Phase 0"));
    }
    Ok(())
}
