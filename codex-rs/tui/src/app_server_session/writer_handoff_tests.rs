use super::*;
use codex_app_server_protocol::JSONRPCErrorError;
use std::path::PathBuf;

fn server_error(code: i64, message: &str) -> TypedRequestError {
    TypedRequestError::Server {
        method: "thread/resume".to_string(),
        source: JSONRPCErrorError {
            code,
            data: None,
            message: message.to_string(),
        },
    }
}

#[test]
fn active_writer_conflict_is_recognised_only_for_the_writer_lock() {
    assert!(is_active_writer_conflict(&server_error(
        -32600,
        "thread 01a08109-3379-74d2-902d-f2f6cf99ee0d already has an active writer"
    )));
    assert!(!is_active_writer_conflict(&server_error(
        -32600,
        "thread not found: 01a08109-3379-74d2-902d-f2f6cf99ee0d"
    )));
    assert!(!is_active_writer_conflict(&server_error(
        -32603,
        "thread 01a08109-3379-74d2-902d-f2f6cf99ee0d already has an active writer"
    )));
    assert!(!is_active_writer_conflict(&TypedRequestError::Transport {
        method: "thread/resume".to_string(),
        source: std::io::Error::other("broken pipe"),
    }));
}

#[cfg(unix)]
fn write_handoff_script(codex_home: &Path, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let scripts = codex_home.join("scripts");
    std::fs::create_dir_all(&scripts).expect("create scripts dir");
    let path = scripts.join("thread-writer-handoff");
    std::fs::write(&path, body).expect("write handoff script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("make handoff script executable");
    path
}

#[cfg(unix)]
#[tokio::test]
async fn take_over_thread_passes_the_thread_id_to_the_handoff_script() {
    let codex_home = tempfile::tempdir().expect("tempdir");
    let record = codex_home.path().join("handoff-args");
    write_handoff_script(
        codex_home.path(),
        &format!("#!/bin/sh\nprintf '%s' \"$1\" > '{}'\n", record.display()),
    );
    let thread_id = ThreadId::new();

    take_over_thread(codex_home.path(), thread_id)
        .await
        .expect("handoff succeeds");

    assert_eq!(
        std::fs::read_to_string(record).expect("script recorded its argument"),
        thread_id.to_string()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn take_over_thread_reports_the_scripts_refusal() {
    let codex_home = tempfile::tempdir().expect("tempdir");
    write_handoff_script(
        codex_home.path(),
        "#!/bin/sh\necho 'thread is mid-turn in Codex pid 42' >&2\nexit 1\n",
    );

    let err = take_over_thread(codex_home.path(), ThreadId::new())
        .await
        .expect_err("a refusal is an error");

    assert!(err.contains("thread is mid-turn in Codex pid 42"), "{err}");
}

#[tokio::test]
async fn take_over_thread_without_a_script_fails_loudly() {
    let codex_home = tempfile::tempdir().expect("tempdir");

    let err = take_over_thread(codex_home.path(), ThreadId::new())
        .await
        .expect_err("a missing script is an error");

    assert!(err.contains("scripts/thread-writer-handoff"), "{err}");
}
