# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "requests",
# ]
# ///

import socket
import os
import json
import threading
import time
import uuid
import logging
import signal
import atexit
import sys
import subprocess
from typing import Dict, Any, Optional

# --- Dependency Check ---
try:
    import requests
except ImportError:
    print("Error: The 'requests' library is not installed. Please install it with 'pip install requests'", file=sys.stderr)
    sys.exit(1)


# --- Global State ---

# A thread-safe dictionary to keep track of active polling tasks.
# Key: task_id (str), Value: a dictionary containing the thread and other metadata
active_tasks: Dict[str, Dict[str, Any]] = {}
# A lock to ensure thread-safe access to the active_tasks dictionary.
tasks_lock = threading.Lock()

# --- Constants ---

SOCKET_PATH = "/tmp/ci_monitor.sock"
PID_FILE_PATH = "/tmp/ci_monitor.pid"
LOG_FILE_PATH = "/tmp/ci_monitor.log"
POLLING_INTERVAL = 60  # seconds
POLLING_TIMEOUT = 3600 # seconds (1 hour)
GITLAB_API_URL = os.getenv("GITLAB_API_URL", "https://gitlab.com/api/v4")

# --- Daemon Setup and Lifecycle ---

def setup_daemon() -> None:
    """
    Initializes the daemon's environment.

    This includes setting up logging, creating the PID file, registering cleanup
    functions, and setting up signal handlers for graceful shutdown.
    """
    logging.basicConfig(
        level=logging.INFO,
        format='%(asctime)s - %(name)s - %(levelname)s - %(message)s',
        filename=LOG_FILE_PATH,
        filemode='w' # Use 'w' to clear the log on each run for easier testing
    )
    logging.info("Daemon starting up.")
    create_pid_file()
    atexit.register(cleanup)
    signal.signal(signal.SIGINT, signal_handler)
    signal.signal(signal.SIGTERM, signal_handler)
    try:
        if os.path.exists(SOCKET_PATH):
            os.unlink(SOCKET_PATH)
    except OSError as e:
        logging.error(f"Error removing old socket file: {e}")
        raise

def create_pid_file() -> None:
    """Creates a file containing the daemon's process ID."""
    try:
        pid = os.getpid()
        with open(PID_FILE_PATH, 'w') as f:
            f.write(str(pid))
        logging.info(f"PID file created at {PID_FILE_PATH} with PID {pid}.")
    except IOError as e:
        logging.error(f"Unable to create PID file: {e}")
        raise SystemExit(f"Fatal: Could not create PID file at {PID_FILE_PATH}.")


def cleanup() -> None:
    """Cleans up resources upon daemon shutdown."""
    logging.info("Daemon shutting down. Cleaning up resources.")
    try:
        if os.path.exists(PID_FILE_PATH):
            os.unlink(PID_FILE_PATH)
            logging.info(f"Removed PID file: {PID_FILE_PATH}")
    except OSError as e:
        logging.error(f"Error removing PID file: {e}")

    try:
        if os.path.exists(SOCKET_PATH):
            os.unlink(SOCKET_PATH)
            logging.info(f"Removed socket file: {SOCKET_PATH}")
    except OSError as e:
        logging.error(f"Error removing socket file: {e}")


def signal_handler(signum: int, frame: Any) -> None:
    """Handles process signals for graceful shutdown."""
    logging.info(f"Received signal {signal.strsignal(signum)}. Initiating graceful shutdown.")
    with tasks_lock:
        if not active_tasks:
            logging.info("No active tasks to stop.")
        else:
            logging.info(f"Stopping {len(active_tasks)} active polling thread(s)...")
            for task_id, task_info in list(active_tasks.items()):
                stop_event = task_info.get('stop_event')
                if stop_event:
                    stop_event.set()
    sys.exit(0)


def run_daemon() -> None:
    """The main entry point for the daemon's server loop."""
    server_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        server_socket.bind(SOCKET_PATH)
        server_socket.listen(5)
        logging.info(f"Daemon listening on {SOCKET_PATH}")
        while True:
            client_socket, address = server_socket.accept()
            logging.info("Accepted connection from a client.")
            handle_client_connection(client_socket)
    except Exception as e:
        logging.critical(f"A fatal error occurred in the main daemon loop: {e}", exc_info=True)
    finally:
        server_socket.close()
        logging.info("Server socket closed.")


# --- Socket Communication and Request Handling ---

def handle_client_connection(client_socket: socket.socket) -> None:
    """Manages a single client connection."""
    try:
        raw_data = client_socket.recv(1024)
        if not raw_data:
            logging.warning("Received empty request from client.")
            return

        request_str = raw_data.decode('utf-8')
        logging.info(f"Received request: {request_str}")

        try:
            payload = json.loads(request_str)
        except json.JSONDecodeError:
            logging.error("Failed to decode JSON from request.")
            response = {"status": "error", "message": "Invalid JSON format."}
        else:
            command = payload.get("command")
            if command == "REGISTER_CI":
                response = handle_register_ci(payload)
            elif command == "CANCEL_CI":
                response = handle_cancel_ci(payload)
            else:
                logging.warning(f"Received unknown command: {command}")
                response = {"status": "error", "message": f"Unknown command: {command}"}

        client_socket.sendall(json.dumps(response).encode('utf-8'))
    except Exception as e:
        logging.error(f"An error occurred while handling a client connection: {e}", exc_info=True)
        try:
            error_response = {"status": "error", "message": "An internal server error occurred."}
            client_socket.sendall(json.dumps(error_response).encode('utf-8'))
        except socket.error as se:
            logging.error(f"Failed to send error response to client: {se}")
    finally:
        client_socket.close()
        logging.info("Client connection closed.")


def handle_register_ci(payload: Dict[str, Any]) -> Dict[str, Any]:
    """Handles a 'REGISTER_CI' request."""
    logging.info(f"Handling REGISTER_CI request: {payload}")
    required_fields = ["merge_request_id", "project_id", "tmux_pane_id"]
    if not all(field in payload for field in required_fields):
        return {"status": "error", "message": "Missing required fields."}

    tmux_pane_id = payload["tmux_pane_id"]

    initial_pane_content = capture_tmux_pane(tmux_pane_id)
    if initial_pane_content is None:
        msg = f"Failed to capture content of tmux pane {tmux_pane_id}."
        logging.error(msg)
        return {"status": "error", "message": msg}

    task_id = str(uuid.uuid4())
    stop_event = threading.Event()
    thread = threading.Thread(
        target=poll_ci_status,
        args=(
            task_id, 
            payload["merge_request_id"], 
            payload["project_id"], 
            tmux_pane_id, 
            stop_event,
            initial_pane_content
        ),
        daemon=True
    )
    thread.start()

    with tasks_lock:
        active_tasks[task_id] = {
            "thread": thread,
            "stop_event": stop_event,
            "merge_request_id": payload["merge_request_id"],
            "project_id": payload["project_id"],
            "start_time": time.time()
        }
    logging.info(f"Registered and started polling task {task_id} for MR {payload['merge_request_id']}.")
    return {"status": "success", "task_id": task_id}


def handle_cancel_ci(payload: Dict[str, Any]) -> Dict[str, Any]:
    """Handles a 'CANCEL_CI' request."""
    logging.info(f"Handling CANCEL_CI request: {payload}")
    required_fields = ["merge_request_id", "project_id"]
    if not all(field in payload for field in required_fields):
        return {"status": "error", "message": "Missing required fields."}
        
    mr_id = payload["merge_request_id"]
    proj_id = payload["project_id"]
    
    tasks_to_cancel = [
        task_id for task_id, info in active_tasks.items()
        if info["merge_request_id"] == mr_id and info["project_id"] == proj_id
    ]

    if not tasks_to_cancel:
        msg = f"No active polling task found for MR {mr_id} and Project {proj_id}."
        logging.warning(msg)
        return {"status": "error", "message": msg}

    for task_id in tasks_to_cancel:
        with tasks_lock:
            if task_id in active_tasks:
                task_info = active_tasks.pop(task_id)
                task_info['stop_event'].set()
                logging.info(f"Cancelled task {task_id} for MR {mr_id}.")
    
    return {"status": "success", "message": f"Cancelled {len(tasks_to_cancel)} matching task(s)."}


# --- CI Polling Logic ---

def poll_ci_status(
    task_id: str,
    merge_request_iid: str,
    project_id: str,
    tmux_pane_id: str,
    stop_event: threading.Event,
    initial_pane_content: str
) -> None:
    """Periodically polls the CI status of a GitLab merge request."""
    logger = logging.getLogger(f"Task-{task_id[:8]}")
    logger.info(f"Started polling for MR !{merge_request_iid} in project {project_id}.")
    
    start_time = time.time()
    completion_states = {"success", "failed", "canceled", "skipped"}
    final_status = None
    
    while not stop_event.is_set():
        if time.time() - start_time > POLLING_TIMEOUT:
            logger.warning(f"Task {task_id} timed out.")
            final_status = "TIMEOUT"
            break

        ci_status = get_ci_status(project_id, merge_request_iid)
        if ci_status:
            logger.info(f"Polled status for MR !{merge_request_iid}: {ci_status}")
            if ci_status in completion_states:
                logger.info(f"CI for MR !{merge_request_iid} completed with status: {ci_status}")
                final_status = ci_status.upper()
                break
        else:
            logger.warning(f"Failed to retrieve CI status for MR !{merge_request_iid}. Will retry.")
        
        stop_event.wait(timeout=POLLING_INTERVAL)

    if final_status and not stop_event.is_set():
        current_pane_content = capture_tmux_pane(tmux_pane_id)
        if current_pane_content is None:
            logger.error(f"Could not capture final pane content for {tmux_pane_id}. Skipping notification.")
        elif current_pane_content == initial_pane_content:
            logger.info(f"Pane content for {tmux_pane_id} is unchanged. Sending in-pane notification.")
            send_tmux_notification(tmux_pane_id, final_status, merge_request_iid)
        else:
            logger.warning(f"Pane content for {tmux_pane_id} has changed. Sending desktop notification.")
            send_desktop_notification(tmux_pane_id, final_status, merge_request_iid)

    with tasks_lock:
        if active_tasks.pop(task_id, None):
            logger.info(f"Cleaned up completed task {task_id}.")
        else:
            logger.info("Task was already cancelled and removed.")


def get_ci_status(project_id: str, merge_request_iid: str) -> Optional[str]:
    """Fetches the CI status for a specific merge request from the GitLab API."""
    gitlab_token = os.getenv("GITLAB_TOKEN")
    if not gitlab_token:
        logging.error("GITLAB_TOKEN environment variable not set.")
        return None

    api_url = f"{GITLAB_API_URL}/projects/{project_id}/merge_requests/{merge_request_iid}"
    headers = {"PRIVATE-TOKEN": gitlab_token}

    try:
        response = requests.get(api_url, headers=headers, timeout=15)
        response.raise_for_status()
        data = response.json()
        pipeline_info = data.get("head_pipeline")
        if not pipeline_info:
            return "pending"
        return pipeline_info.get("status")
    except requests.exceptions.RequestException as e:
        logging.error(f"Network error fetching MR data: {e}")
    return None


# --- Notification & Tmux Helpers ---

def capture_tmux_pane(tmux_pane_id: str) -> Optional[str]:
    """Captures the visible contents of a tmux pane."""
    logger = logging.getLogger("TmuxHelper")
    try:
        tmux_command = ["tmux", "capture-pane", "-p", "-t", tmux_pane_id]
        result = subprocess.run(tmux_command, capture_output=True, text=True, check=True)
        return result.stdout
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        logger.error(f"Failed to capture tmux pane {tmux_pane_id}: {e}")
    return None

def send_tmux_notification(
    tmux_pane_id: str, ci_status: str, merge_request_id: str
) -> None:
    """Sends a notification command directly to the specified tmux pane."""
    logger = logging.getLogger("Notification")
    try:
        command_to_run = f'Pipeline for MR {merge_request_id} has finished running. Status is {ci_status}'
        tmux_command = ["tmux", "send-keys", "-t", tmux_pane_id, command_to_run, "C-m"]
        subprocess.run(tmux_command, check=True)
        logger.info(f"Successfully sent in-pane notification to {tmux_pane_id}.")
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        logger.error(f"Failed to send in-pane notification: {e}")

def send_desktop_notification(tmux_pane_id: str, ci_status: str, merge_request_id: str) -> None:
    """Sends a macOS desktop notification with a callback action."""
    logger = logging.getLogger("Notification")
    if sys.platform != "darwin":
        logger.warning("Desktop notifications are only supported on macOS. Skipping.")
        return

    title = f"CI finished: {ci_status}"
    message = "Agent pane has changed since CI watch"
    action = "Focus"
    
    try:
        alerter_command = [
            "alerter",
            "-title", title,
            "-message", message,
            "-actions", action,
        ]
        clicked_action = subprocess.run(
            alerter_command, capture_output=True, text=True, check=True
        ).stdout.strip()

        if clicked_action == action:
            logger.info(f"User clicked '{action}'. Focusing pane {tmux_pane_id}.")
            focus_tmux_pane(tmux_pane_id)
        else:
            logger.info("Desktop notification was closed without action.")

    except FileNotFoundError:
        logger.error("`alerter` command not found. Please install with 'brew install alerter'.")
    except subprocess.CalledProcessError as e:
        logger.error(f"Alerter command failed: {e.stderr.strip()}")

def focus_tmux_pane(pane_id: str) -> None:
    """
    Brings the terminal to the front and focuses the specified tmux pane.
    This is more robust than simulating keystrokes for the tmux prefix.
    """
    logger = logging.getLogger("TmuxHelper")
    
    # Step 1: Use AppleScript to simply activate the terminal application.
    # This brings the application to the foreground.
    activate_script = '''
 on run
    tell application "Kitty" to activate
end run
'''
    try:
        subprocess.run(["osascript", "-e", activate_script], check=True)
        logger.info("Successfully activated Kitty terminal.")
        # Give the OS a moment to switch focus
        time.sleep(0.2)
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        logger.error(f"Failed to execute AppleScript to activate Kitty: {e}")
        return # If we can't activate the app, don't proceed

    # Step 2: Use a direct tmux command to switch to the correct pane.
    # This is independent of keyboard mappings and prefixes.
    try:
        # 'switch-client' is ideal as it changes the focused session/window/pane
        # for the client running in the terminal.
        tmux_command = ["tmux", "switch-client", "-t", pane_id]
        subprocess.run(tmux_command, check=True)
        logger.info(f"Successfully sent command to switch to pane {pane_id}.")
    except (FileNotFoundError, subprocess.CalledProcessError) as e:
        logger.error(f"Failed to execute tmux command to switch pane: {e}")


# --- Main Execution ---

def main() -> None:
    """Main function to start the CI Monitoring Daemon."""
    if not os.getenv("GITLAB_TOKEN"):
        print("Error: The GITLAB_TOKEN environment variable must be set.", file=sys.stderr)
        sys.exit(1)
    
    try:
        setup_daemon()
        run_daemon()
    except Exception as e:
        logging.critical(f"A critical error occurred: {e}", exc_info=True)
        sys.exit(1)


if __name__ == "__main__":
    main()

