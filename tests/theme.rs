use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn popup_forwards_the_selected_theme_to_the_tmux_server() {
    let directory = tempfile::tempdir().unwrap();
    let tmux = directory.path().join("tmux");
    fs::write(&tmux, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$TRACE\"\n").unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();
    let trace = directory.path().join("trace");
    for value in [None, Some("terminal"), Some(" EVERFOREST "), Some("tmux")] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_rollcall"));
        command
            .arg("popup")
            .env("TMUX", "test-socket,123,0")
            .env("ROLLCALL_TMUX_CLIENT", "/dev/pts/origin")
            .env_remove("ROLLCALL_PICK_SELECTION")
            .env("TRACE", &trace)
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    directory.path().display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env_remove("ROLLCALL_THEME");
        if let Some(value) = value {
            command.env("ROLLCALL_THEME", value);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args = fs::read_to_string(&trace).unwrap();
        let expected = match value {
            Some(" EVERFOREST ") => "everforest",
            Some("tmux") => "tmux",
            _ => "terminal",
        };
        assert!(
            args.contains(&format!("-e\nROLLCALL_THEME={expected}\n")),
            "{args}"
        );
    }
}

#[test]
fn invalid_theme_fails_before_opening_the_terminal_or_starting_probes() {
    for command in ["pick", "popup"] {
        let output = Command::new(env!("CARGO_BIN_EXE_rollcall"))
            .arg(command)
            .env("ROLLCALL_THEME", "typo")
            .env_remove("TMUX")
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("ROLLCALL_THEME must be terminal, everforest, or tmux")
        );
        assert!(output.stdout.is_empty());
    }
}
