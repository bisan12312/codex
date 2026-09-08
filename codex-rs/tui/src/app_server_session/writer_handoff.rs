//! Takes a thread over from another idle Codex process on this machine.
//!
//! A thread's writer lock (`$CODEX_HOME/thread-writer-locks/<id>.lock`) lives as
//! long as the Codex process that opened the thread, so resuming a chat that a
//! Codex Phone tmux pane still holds fails with "already has an active writer".
//! The project-owned script behind `$CODEX_HOME/scripts/thread-writer-handoff`
//! asks that Codex to `/exit` while the thread is idle and refuses when it is
//! mid-turn; `resume_thread` retries the request once it succeeds.

use std::path::Path;
use std::time::Duration;

use codex_app_server_client::TypedRequestError;
use codex_protocol::ThreadId;

use super::JSONRPC_INVALID_REQUEST;

const HANDOFF_SCRIPT: &str = "scripts/thread-writer-handoff";
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(90);

pub(super) fn is_active_writer_conflict(err: &TypedRequestError) -> bool {
    match err {
        TypedRequestError::Server { source, .. } => {
            source.code == JSONRPC_INVALID_REQUEST
                && source.message.contains("already has an active writer")
        }
        TypedRequestError::Transport { .. } | TypedRequestError::Deserialize { .. } => false,
    }
}

pub(super) async fn take_over_thread(codex_home: &Path, thread_id: ThreadId) -> Result<(), String> {
    let script_path = codex_home.join(HANDOFF_SCRIPT);
    if !script_path.is_file() {
        return Err(format!(
            "another Codex still holds thread {thread_id} and there is no handoff script at {}",
            script_path.display()
        ));
    }
    let mut command = tokio::process::Command::new(&script_path);
    command.arg(thread_id.to_string());
    command.kill_on_drop(true);
    let output = tokio::time::timeout(HANDOFF_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "thread-writer handoff for {thread_id} exceeded {HANDOFF_TIMEOUT:?}: {}",
                script_path.display()
            )
        })?
        .map_err(|err| {
            format!(
                "failed to run thread-writer handoff {}: {err}",
                script_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "could not take thread {thread_id} over ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    tracing::info!(
        %thread_id,
        output = %String::from_utf8_lossy(&output.stdout).trim(),
        "thread-writer handoff completed"
    );
    Ok(())
}

#[cfg(test)]
#[path = "writer_handoff_tests.rs"]
mod tests;
