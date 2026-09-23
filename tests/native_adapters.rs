use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Command, Output},
};

use serde_json::{Value, json};

const ID: &str = "11111111-1111-4111-8111-111111111111";

fn row(agent: &str) -> Value {
    json!({
        "session": {"id":"", "host":"", "agent":agent, "nativeSessionId":ID,
            "title":"Fixture", "cwd":"/work/a project", "source":agent, "activity":"unknown",
            "lastMessage":"", "lastInteractionUnixSeconds":100, "updatedUnixSeconds":100,
            "runtime":"resumable"},
        "locator":"/config/Claude's directory", "rawId":ID, "profile":"a project 'quoted'",
        "ownershipUncertain":false
    })
}

fn invoke(row: Value, action: &str, remote: bool) -> (Output, String) {
    let root = tempfile::tempdir().unwrap();
    // The real Rust dispatch and quoting run, but no harness, tmux server, or
    // remote connection is started. SSH probes and attachment are distinguished.
    for (name, script) in [
        ("bash", "#!/bin/sh\nprintf '%s' \"$INVENTORY\"\n"),
        (
            "ssh",
            "#!/bin/sh\ncase \"$*\" in *python3*) printf '%s' \"$INVENTORY\";; *) printf '%s\\n' \"$@\" > \"$TRACE\";; esac\n",
        ),
        ("tmux", "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$TRACE\"\n"),
    ] {
        let path = root.path().join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let host = if remote { "fixture-host" } else { "local" };
    let id = format!("{host}:{}:{ID}", row["session"]["agent"].as_str().unwrap());
    let trace = root.path().join("trace");
    let output = Command::new(env!("CARGO_BIN_EXE_rollcall"))
        .args([action, &id])
        .env(
            "PATH",
            format!(
                "{}:{}",
                root.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .env("TRACE", &trace)
        .env("ROLLCALL_STATE_PATH", root.path().join("state.db"))
        .env("INVENTORY", serde_json::to_string(&vec![row]).unwrap())
        .env_remove("TMUX")
        .env_remove("ROLLCALL_TMUX_CLIENT")
        .env_remove("ROLLCALL_PICK_SELECTION")
        .output()
        .unwrap();
    (output, fs::read_to_string(trace).unwrap_or_default())
}

#[test]
fn new_adapters_dispatch_exact_local_resume_commands() {
    for agent in ["claude", "agy"] {
        let (output, trace) = invoke(row(agent), "attach", false);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let args: Vec<_> = trace.lines().collect();
        assert_eq!(&args[..3], &["new-session", "-A", "-s"]);
        assert_eq!(args[5], "/work/a project");
        let login = shlex::split(args[6]).unwrap();
        assert_eq!(&login[..3], &["exec", "bash", "-lc"]);
        let command = shlex::split(&login[3]).unwrap();
        if agent == "claude" {
            assert_eq!(
                command,
                [
                    "env",
                    "CLAUDE_CONFIG_DIR=/config/Claude's directory",
                    "claude",
                    "--resume",
                    ID
                ]
            );
        } else {
            assert_eq!(
                command,
                [
                    "agy",
                    "--conversation",
                    ID,
                    "--project",
                    "a project 'quoted'"
                ]
            );
        }
    }
}

#[test]
fn uncertain_ownership_blocks_attach_even_with_a_tmux_binding_but_allows_explicit_resume() {
    for agent in ["claude", "agy"] {
        let mut candidate = row(agent);
        candidate["ownershipUncertain"] = json!(true);
        candidate["session"]["tmux"] = json!({"session":"existing", "pane":"%9"});
        let (output, trace) = invoke(candidate.clone(), "attach", false);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("refusing an implicit second writer")
        );
        assert!(trace.is_empty());
        let (output, trace) = invoke(candidate, "resume", false);
        assert!(output.status.success());
        assert!(trace.starts_with("new-session\n"));
    }
}

#[test]
fn agy_remote_resume_uses_existing_ssh_transport_and_login_shell() {
    let (output, trace) = invoke(row("agy"), "resume", true);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let args: Vec<_> = trace.lines().collect();
    assert_eq!(&args[..3], &["-t", "--", "fixture-host"]);
    let login = shlex::split(args[3]).unwrap();
    assert_eq!(&login[..2], &["bash", "-lc"]);
    let tmux = shlex::split(&login[2]).unwrap();
    let inner = shlex::split(tmux.last().unwrap()).unwrap();
    let command = shlex::split(&inner[3]).unwrap();
    assert_eq!(
        command,
        [
            "agy",
            "--conversation",
            ID,
            "--project",
            "a project 'quoted'"
        ]
    );
}

#[test]
fn missing_workspace_does_not_resume_in_a_guessed_directory() {
    let mut candidate = row("agy");
    candidate["session"]["cwd"] = json!("");
    let (output, trace) = invoke(candidate, "resume", false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no known workspace"));
    assert!(trace.is_empty());
}
