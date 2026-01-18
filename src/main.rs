use anyhow::{anyhow, Context, Result};
use clap::{Parser, ValueEnum};
use dashmap::DashMap;
use log::{error, info, warn};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Command;
use tokio::signal;
use tokio::sync::broadcast;
use tokio::time::timeout;

// --- Constants ---
const SOCKET_PATH: &str = "/tmp/ci_monitor.sock";
const PID_FILE_PATH: &str = "/tmp/ci_monitor.pid";
const POLLING_INTERVAL_SECS: u64 = 30;
const POLLING_TIMEOUT_SECS: u64 = 3600;

// --- CLI Arguments ---

#[derive(Parser, Debug)]
#[command(name = "ci_monitor_daemon")]
#[command(about = "A daemon for monitoring GitLab CI/CD pipelines")]
struct Args {
    /// Notification behavior
    #[arg(long, value_enum, default_value_t = NotifyMode::Always)]
    notify: NotifyMode,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum NotifyMode {
    /// Never send notifications
    Never,
    /// Only notify when pane contents have changed
    PaneContentsChanged,
    /// Always send notifications
    Always,
}

// --- Global State ---

// Key for tracking unique pipelines
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct PipelineKey {
    project_id: String,
    merge_request_id: String,
}

// A thread-safe hash map to store active polling tasks.
struct ActiveTask {
    // The handle to the spawned tokio task. We can call `.abort()` on this to stop it.
    task_handle: tokio::task::JoinHandle<()>,
}

// Using `LazyLock` for the most ergonomic, built-in lazy initialization.
// This provides the same ease-of-use as the old `lazy_static!` macro.
static ACTIVE_TASKS: LazyLock<DashMap<PipelineKey, Arc<ActiveTask>>> = LazyLock::new(DashMap::new);
static NOTIFY_MODE: LazyLock<std::sync::Mutex<NotifyMode>> = LazyLock::new(|| std::sync::Mutex::new(NotifyMode::Always));
static HTTP_CLIENT: LazyLock<Client> = LazyLock::new(Client::new);
static GITLAB_API_URL: LazyLock<String> = LazyLock::new(|| {
    env::var("GITLAB_API_URL").unwrap_or_else(|_| "https://gitlab.com/api/v4".to_string())
});


// --- Request/Response Data Structures ---

#[derive(Deserialize)]
struct RegisterRequest {
    project_id: String,
    merge_request_id: String,
    tmux_pane_id: String,
}

#[derive(Deserialize)]
struct CancelRequest {
    project_id: String,
    merge_request_id: String,
}

#[derive(Deserialize)]
#[serde(tag = "command")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
enum CommandPayload {
    RegisterCi(RegisterRequest),
    CancelCi(CancelRequest),
}

#[derive(Serialize)]
struct Response {
    status: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_id: Option<String>,
}

// --- Main Execution & Daemon Lifecycle ---

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    
    // Set the global notification mode
    {
        let mut notify_mode = NOTIFY_MODE.lock().unwrap();
        *notify_mode = args.notify;
    }
    
    if env::var("GITLAB_TOKEN").is_err() {
        eprintln!("Error: The GITLAB_TOKEN environment variable must be set.");
        std::process::exit(1);
    }

    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // The RAII guard ensures cleanup runs when main exits.
    let _cleanup_guard = scopeguard::guard((), |_| {
        info!("Daemon shutting down. Cleaning up resources.");
        if let Err(e) = std::fs::remove_file(PID_FILE_PATH) {
            warn!("Failed to remove PID file: {}", e);
        }
        if let Err(e) = std::fs::remove_file(SOCKET_PATH) {
            warn!("Failed to remove socket file: {}", e);
        }
    });

    setup_daemon_environment().await?;

    let (shutdown_tx, mut shutdown_rx) = broadcast::channel(1);

    info!("Daemon listening on {}", SOCKET_PATH);
    let listener = UnixListener::bind(SOCKET_PATH)
        .with_context(|| format!("Failed to bind to socket: {}", SOCKET_PATH))?;

    loop {
        tokio::select! {
            Ok((socket, _)) = listener.accept() => {
                info!("Accepted connection from a client.");
                tokio::spawn(handle_client_connection(socket));
            }
            _ = shutdown_rx.recv() => {
                info!("Shutdown signal received. Exiting main loop.");
                break;
            }
            _ = signal::ctrl_c() => {
                info!("Ctrl+C received. Initiating graceful shutdown.");
                let _ = shutdown_tx.send(());
                break;
            }
        }
    }

    // Abort all running tasks before exiting.
    info!("Stopping {} active polling task(s)...", ACTIVE_TASKS.len());
    ACTIVE_TASKS.iter().for_each(|task| {
        task.value().task_handle.abort();
    });
    ACTIVE_TASKS.clear();

    Ok(())
}

async fn setup_daemon_environment() -> Result<()> {
    info!("Daemon starting up.");
    let pid = std::process::id();
    std::fs::write(PID_FILE_PATH, pid.to_string())
        .with_context(|| format!("Could not create PID file at {}", PID_FILE_PATH))?;
    info!("PID file created at {} with PID {}.", PID_FILE_PATH, pid);

    if Path::new(SOCKET_PATH).exists() {
        std::fs::remove_file(SOCKET_PATH)
            .with_context(|| format!("Could not remove old socket file: {}", SOCKET_PATH))?;
    }

    Ok(())
}

// --- Socket Communication and Request Handling ---

async fn handle_client_connection(mut socket: UnixStream) {
    let mut buffer = vec![0; 1024];
    match socket.read(&mut buffer).await {
        Ok(size) if size > 0 => {
            let request_str = String::from_utf8_lossy(&buffer[..size]);
            info!("Received request: {}", request_str);

            let response = match serde_json::from_str::<CommandPayload>(&request_str) {
                Ok(CommandPayload::RegisterCi(payload)) => handle_register_ci(payload).await,
                Ok(CommandPayload::CancelCi(payload)) => handle_cancel_ci(payload).await,
                Err(e) => {
                    warn!("Failed to decode JSON or unknown command: {}", e);
                    Response {
                        status: "error".to_string(),
                        message: "Invalid JSON or unknown command".to_string(),
                        task_id: None,
                    }
                }
            };

            let response_json = serde_json::to_string(&response).unwrap_or_else(|e| {
                error!("Failed to serialize response: {}", e);
                "{\"status\":\"error\",\"message\":\"Internal server error during response serialization.\"}"
                    .to_string()
            });

            if let Err(e) = socket.write_all(response_json.as_bytes()).await {
                error!("Failed to send response to client: {}", e);
            }
        }
        Ok(_) => warn!("Received empty request from client."),
        Err(e) => error!("Failed to read from socket: {}", e),
    }
}

async fn handle_register_ci(payload: RegisterRequest) -> Response {
    info!(
        "Handling REGISTER_CI for MR {} in project {}",
        payload.merge_request_id, payload.project_id
    );

    let pipeline_key = PipelineKey {
        project_id: payload.project_id.clone(),
        merge_request_id: payload.merge_request_id.clone(),
    };

    // Cancel existing task for this pipeline if it exists
    if let Some((_, existing_task)) = ACTIVE_TASKS.remove(&pipeline_key) {
        existing_task.task_handle.abort();
        info!("Cancelled existing task for MR {} in project {}", payload.merge_request_id, payload.project_id);
    }

    let project_id_clone = payload.project_id.clone();
    let mr_id_clone = payload.merge_request_id.clone();
    let tmux_pane_id_clone = payload.tmux_pane_id.clone();
    let pipeline_key_clone = pipeline_key.clone();

    // Spawn the polling task immediately. It will handle its own internal delay.
    let task_handle = tokio::spawn(async move {
        poll_ci_status(
            pipeline_key_clone,
            project_id_clone,
            mr_id_clone,
            tmux_pane_id_clone,
        )
        .await;
    });

    let task = Arc::new(ActiveTask {
        task_handle,
    });

    ACTIVE_TASKS.insert(pipeline_key.clone(), task);

    let task_id_str = format!("{}-{}", payload.project_id, payload.merge_request_id);
    info!("Registered and started polling task {}", task_id_str);
    // Respond to the client immediately.
    Response {
        status: "success".to_string(),
        message: "CI monitoring task registered.".to_string(),
        task_id: Some(task_id_str),
    }
}

async fn handle_cancel_ci(payload: CancelRequest) -> Response {
    info!(
        "Handling CANCEL_CI for MR {} in project {}",
        payload.merge_request_id, payload.project_id
    );

    let pipeline_key = PipelineKey {
        project_id: payload.project_id.clone(),
        merge_request_id: payload.merge_request_id.clone(),
    };

    if let Some((_, task)) = ACTIVE_TASKS.remove(&pipeline_key) {
        task.task_handle.abort();
        let task_id_str = format!("{}-{}", payload.project_id, payload.merge_request_id);
        info!("Cancelled task {}", task_id_str);
        Response {
            status: "success".to_string(),
            message: "Cancelled matching task.".to_string(),
            task_id: None,
        }
    } else {
        let msg = format!(
            "No active polling task found for MR {} and Project {}",
            payload.merge_request_id, payload.project_id
        );
        warn!("{}", msg);
        Response { status: "error".to_string(), message: msg, task_id: None }
    }
}

// --- CI Polling Logic ---

async fn poll_ci_status(
    pipeline_key: PipelineKey,
    project_id: String,
    merge_request_iid: String,
    tmux_pane_id: String,
) {
    let task_name = format!("Task-{}-{}", project_id, merge_request_iid);

    // Wait 15 seconds before doing anything else.
    info!("{}: Waiting 15 seconds before initial pane capture.", task_name);
    tokio::time::sleep(Duration::from_secs(15)).await;

    // Now, capture the initial pane content.
    let initial_pane_content = match capture_tmux_pane(&tmux_pane_id).await {
        Ok(content) => content,
        Err(e) => {
            error!(
                "{}: Could not capture initial pane content for {}. Aborting task. Error: {}",
                task_name, tmux_pane_id, e
            );
            ACTIVE_TASKS.remove(&pipeline_key);
            return;
        }
    };
    
    info!("{}: Initial pane content captured. Starting polling for MR !{} in project {}.", task_name, merge_request_iid, project_id);


    let polling_logic = async {
        loop {
            match get_ci_status(&project_id, &merge_request_iid).await {
                Ok(Some(status)) => {
                    info!("{}: Polled status for MR !{}: {}", task_name, merge_request_iid, status);
                    let completion_states = ["success", "failed", "canceled", "skipped"];
                    if completion_states.contains(&status.as_str()) {
                        info!("{}: CI for MR !{} completed with status: {}", task_name, merge_request_iid, status);
                        return Some(status.to_uppercase());
                    }
                }
                Ok(None) => {
                    info!("{}: No pipeline found yet for MR !{}, assuming pending.", task_name, merge_request_iid);
                }
                Err(e) => {
                    warn!("{}: Failed to retrieve CI status for MR !{}: {}. Will retry.", task_name, merge_request_iid, e);
                }
            }
            tokio::time::sleep(Duration::from_secs(POLLING_INTERVAL_SECS)).await;
        }
    };

    let final_status = match timeout(Duration::from_secs(POLLING_TIMEOUT_SECS), polling_logic).await {
        Ok(Some(status)) => status,
        Ok(None) => "UNKNOWN".to_string(),
        Err(_) => {
            warn!("{} timed out.", task_name);
            "TIMEOUT".to_string()
        }
    };
    
    let current_pane_content = match capture_tmux_pane(&tmux_pane_id).await {
        Ok(content) => content,
        Err(e) => {
            error!("{}: Could not capture final pane content for {}. Skipping notification. Error: {}", task_name, tmux_pane_id, e);
            ACTIVE_TASKS.remove(&pipeline_key);
            return;
        }
    };

    let notify_mode = {
        let mode = NOTIFY_MODE.lock().unwrap();
        *mode
    };
    
    let pane_content_unchanged = current_pane_content.trim() == initial_pane_content.trim();
    
    // Always send tmux notification when pane content hasn't changed
    if pane_content_unchanged {
        info!("{}: Pane content for {} is unchanged. Sending in-pane notification.", task_name, tmux_pane_id);
        if let Err(e) = send_tmux_notification(&tmux_pane_id, &final_status, &merge_request_iid).await {
            error!("{}: Failed to send tmux notification: {}", task_name, e);
        }
    }
    
    // Desktop notifications based on --notify parameter
    match notify_mode {
        NotifyMode::Never => {
            info!("{}: Desktop notifications disabled.", task_name);
        }
        NotifyMode::PaneContentsChanged => {
            if !pane_content_unchanged {
                warn!("{}: Pane content for {} has changed. Sending desktop notification.", task_name, tmux_pane_id);
                if let Err(e) = send_desktop_notification(&tmux_pane_id, &final_status, &merge_request_iid, false).await {
                    error!("{}: Failed to send desktop notification: {}", task_name, e);
                }
            }
        }
        NotifyMode::Always => {
            if pane_content_unchanged {
                info!("{}: Pane content unchanged. Sending desktop notification with agent prompt.", task_name);
                if let Err(e) = send_desktop_notification(&tmux_pane_id, &final_status, &merge_request_iid, true).await {
                    error!("{}: Failed to send desktop notification: {}", task_name, e);
                }
            } else {
                warn!("{}: Pane content for {} has changed. Sending desktop notification.", task_name, tmux_pane_id);
                if let Err(e) = send_desktop_notification(&tmux_pane_id, &final_status, &merge_request_iid, false).await {
                    error!("{}: Failed to send desktop notification: {}", task_name, e);
                }
            }
        }
    }

    ACTIVE_TASKS.remove(&pipeline_key);
    info!("{}: Cleaned up completed task.", task_name);
}

#[derive(Deserialize)]
struct Pipeline {
    status: String,
}
#[derive(Deserialize)]
struct GitLabMergeRequest {
    head_pipeline: Option<Pipeline>,
}

async fn get_ci_status(project_id: &str, merge_request_iid: &str) -> Result<Option<String>> {
    let gitlab_token = env::var("GITLAB_TOKEN").context("GITLAB_TOKEN environment variable not set")?;
    let api_url = format!("{}/projects/{}/merge_requests/{}", &*GITLAB_API_URL, project_id, merge_request_iid);

    let response = HTTP_CLIENT
        .get(&api_url)
        .header("PRIVATE-TOKEN", gitlab_token)
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .context("Network error fetching MR data")?;

    if !response.status().is_success() {
        return Err(anyhow!("GitLab API returned an error: {} - {}", response.status(), response.text().await.unwrap_or_default()));
    }

    let data = response.json::<GitLabMergeRequest>().await?;

    Ok(data.head_pipeline.map(|p| p.status))
}


// --- Notification & Tmux Helpers ---

async fn capture_tmux_pane(tmux_pane_id: &str) -> Result<String> {
    let output = Command::new("tmux")
        .args(["capture-pane", "-p", "-t", tmux_pane_id])
        .output()
        .await
        .context("Failed to execute tmux command")?;

    if !output.status.success() {
        return Err(anyhow!(
            "tmux capture-pane failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8(output.stdout)?)
}

async fn send_tmux_notification(tmux_pane_id: &str, ci_status: &str, merge_request_id: &str) -> Result<()> {
    // Construct the message without a newline.
    let message = format!("Pipeline for MR {} finished with status: {}", merge_request_id, ci_status);

    // Send the message string, and then send the "Enter" key separately.
    // This more accurately simulates a user pressing the Enter key.
    let status = Command::new("tmux")
        .args(["send-keys", "-t", tmux_pane_id, &message, "Enter"])
        .status()
        .await
        .context("Failed to execute tmux send-keys")?;

    if !status.success() {
        return Err(anyhow!("tmux send-keys command reported a failure. This might happen if the receiving shell exits with an error after processing the input."));
    }

    let status = Command::new("tmux")
        .args(["send-keys", "-t", tmux_pane_id, "Enter"])
        .status()
        .await
        .context("Failed to execute tmux send-keys")?;

    if !status.success() {
        return Err(anyhow!("tmux send-keys command reported a failure. This might happen if the receiving shell exits with an error after processing the input."));
    }
    info!("Successfully sent in-pane notification to {}.", tmux_pane_id);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn send_desktop_notification(_tmux_pane_id: &str, ci_status: &str, _merge_request_id: &str, _agent_prompted: bool) -> Result<()> {
    warn!("Desktop notifications are not supported on this OS. CI Status: {}", ci_status);
    Ok(())
}

#[cfg(target_os = "macos")]
async fn send_desktop_notification(tmux_pane_id: &str, ci_status: &str, merge_request_id: &str, agent_prompted: bool) -> Result<()> {
    let (title, message) = if agent_prompted {
        (format!("MR !{} {}", merge_request_id, ci_status.to_uppercase()), "Agent prompted".to_string())
    } else {
        (format!("MR !{} {}", merge_request_id, ci_status.to_uppercase()), "Pane changed".to_string())
    };
    let action = "Focus";

    let output = Command::new("alerter")
        .args([
            "-title", &title,
            "-message", &message,
            "-actions", action,
            "-timeout", "30",
        ])
        .output()
        .await;

    match output {
        Ok(out) => {
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                return Err(anyhow!("Alerter command failed: {}", stderr));
            }
            let clicked_action = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if clicked_action == action {
                info!("User clicked '{}'. Focusing pane {}.", action, tmux_pane_id);
                focus_tmux_pane(tmux_pane_id).await?;
            } else {
                info!("Desktop notification was closed without action.");
            }
        }
        Err(e) => {
             if e.kind() == std::io::ErrorKind::NotFound {
                 return Err(anyhow!("`alerter` command not found. Please install with 'brew install alerter'."));
             }
             return Err(e).context("Failed to run alerter command");
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
async fn focus_tmux_pane(pane_id: &str) -> Result<()> {
    let activate_script = r#"tell application "Kitty" to activate"#;
    let status = Command::new("osascript")
        .args(["-e", activate_script])
        .status()
        .await
        .context("Failed to execute AppleScript to activate Kitty")?;
        
    if !status.success() {
        return Err(anyhow!("AppleScript to activate terminal failed."));
    }
    
    tokio::time::sleep(Duration::from_millis(200)).await;

    let status = Command::new("tmux")
        .args(["switch-client", "-t", pane_id])
        .status()
        .await
        .context("Failed to execute tmux to switch pane")?;

    if !status.success() {
        return Err(anyhow!("tmux switch-client command failed."));
    }
    info!("Successfully focused tmux pane {}.", pane_id);
    Ok(())
}
