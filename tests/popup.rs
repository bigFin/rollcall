use std::{fs, os::unix::fs::PermissionsExt, process::Command};

fn popup(selection: &str, fail: bool) -> (tempfile::TempDir, std::process::Output, String) {
    let dir = tempfile::tempdir().unwrap();
    let tmux = dir.path().join("tmux");
    fs::write(
        &tmux,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$TRACE"
case "$1" in
  display-popup)
    while [ "$#" -gt 0 ]; do
      case "$1" in ROLLCALL_PICK_SELECTION=*) file=${1#*=};; esac
      shift
    done
    printf '%s' "$file" > "$SELECTION_PATH"
    [ "$FAIL_POPUP" = 1 ] && exit 1
    if [ -n "$PICKED" ]; then printf '"%s"' "$PICKED" > "$file"; fi
    printf 'popup-closed\n' >> "$TRACE"
    ;;
  list-clients) printf '/dev/pts/origin\t$4\t/run/user/1000/tmux/default\n/dev/pts/other\t$9\t/run/user/1000/tmux/default\n';;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();
    let trace = dir.path().join("trace");
    let output = Command::new(env!("CARGO_BIN_EXE_rollcall"))
        .arg("popup")
        .env(
            "PATH",
            format!(
                "{}:{}",
                dir.path().display(),
                std::env::var("PATH").unwrap()
            ),
        )
        .env("TMUX", "test-socket,123,0")
        .env("ROLLCALL_TMUX_CLIENT", "/dev/pts/origin")
        .env_remove("ROLLCALL_PICK_SELECTION")
        .env_remove("ROLLCALL_THEME")
        .env("TRACE", &trace)
        .env("SELECTION_PATH", dir.path().join("selection-path"))
        .env("PICKED", selection)
        .env("FAIL_POPUP", if fail { "1" } else { "0" })
        .env("ROLLCALL_STATE_PATH", dir.path().join("state.db"))
        .output()
        .unwrap();
    let log = fs::read_to_string(trace).unwrap();
    let handoff = fs::read_to_string(dir.path().join("selection-path")).unwrap();
    assert!(
        !std::path::Path::new(&handoff).exists(),
        "handoff file leaked"
    );
    (dir, output, log)
}

#[test]
fn popup_closes_before_switching_only_the_originating_client() {
    let (_dir, output, log) = popup("local:tmux:chosen", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(log.contains("display-popup -E -c /dev/pts/origin"));
    assert!(
        log.contains("popup-closed\nswitch-client -c /dev/pts/origin -t chosen"),
        "{log}"
    );
    assert!(log.contains("ROLLCALL_TMUX_CLIENT=/dev/pts/origin"));
    assert!(!log.contains("new-window"));
}

#[test]
fn remote_selection_replaces_the_origin_client_without_nesting() {
    let (_dir, output, log) = popup("remote-host:tmux:chosen", false);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(log.find("popup-closed").unwrap() < log.find("detach-client").unwrap());
    assert!(
        log.contains(
            "detach-client -t /dev/pts/origin -E env -u TMUX -u TMUX_PANE ssh -t -- remote-host"
        ),
        "{log}"
    );
    assert_eq!(log.matches("bash -lc").count(), 1, "{log}");
    assert!(
        log.contains("tmux -S /run/user/1000/tmux/default attach-session -t '$4'"),
        "{log}"
    );
    assert!(!log.contains("new-window") && !log.contains("switch-client"));
}

#[test]
fn cancellation_never_attaches_or_opens_a_window() {
    let (_dir, output, log) = popup("", false);
    assert!(output.status.success());
    assert!(
        !log.contains("switch-client")
            && !log.contains("new-window")
            && !log.contains("detach-client")
    );
}

#[test]
fn failed_popup_never_attaches() {
    let (_dir, output, log) = popup("local:tmux:chosen", true);
    assert!(!output.status.success());
    assert!(
        !log.contains("switch-client")
            && !log.contains("new-window")
            && !log.contains("detach-client")
    );
}
