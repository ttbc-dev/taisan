use std::process::Command;

#[test]
fn primary_and_legacy_commands_expose_taisan_help() {
    for binary in [
        env!("CARGO_BIN_EXE_taisan-tui"),
        env!("CARGO_BIN_EXE_workloop-tui"),
    ] {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).unwrap();
        assert!(help.contains("Usage: taisan-tui"));
        assert!(help.contains("workloop.db"));
    }
}
