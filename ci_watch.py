#!/usr/bin/env -S uv --quiet run --script

import argparse
import socket
import json
import sys
import os

# The socket path must match the one used by the daemon.
SOCKET_PATH = "/tmp/ci_monitor.sock"

def send_request(payload: dict) -> bool:
    """
    Connects to the daemon via a Unix socket and sends the JSON payload.

    Args:
        payload: The dictionary to be sent as a JSON request.

    Returns:
        True if the daemon responded with a 'success' status, False otherwise.
    """
    # Check if the socket file exists before attempting to connect.
    if not os.path.exists(SOCKET_PATH):
        # Silently fail as per spec (no stdout/stderr). The exit code will signal failure.
        return False

    client_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        # Connect to the daemon's socket.
        client_socket.connect(SOCKET_PATH)

        # Serialize the payload to JSON and send it.
        request_bytes = json.dumps(payload).encode('utf-8')
        client_socket.sendall(request_bytes)

        # Shut down the writing half of the socket to signal the end of the request.
        client_socket.shutdown(socket.SHUT_WR)

        # Receive the response from the daemon.
        response_bytes = client_socket.recv(1024)
        response_str = response_bytes.decode('utf-8')
        
        # An empty response is considered a failure.
        if not response_str:
            return False

        # Parse the JSON response.
        response_data = json.loads(response_str)
        
        # Check if the operation was successful.
        return response_data.get("status") == "success"

    except (socket.error, json.JSONDecodeError, FileNotFoundError):
        # Any exception during communication is considered a failure.
        # We fail silently and return False.
        return False
    finally:
        # Always close the socket.
        client_socket.close()

def main():
    """
    Parses command-line arguments and orchestrates the client's actions.
    """
    parser = argparse.ArgumentParser(
        description="Register or cancel a CI pipeline monitoring task."
    )
    parser.add_argument(
        "--mr-id",
        required=True,
        type=str,
        help="The merge request ID to monitor or cancel."
    )
    parser.add_argument(
        "--project-id",
        required=True,
        type=str,
        help="The project ID for the merge request."
    )
    parser.add_argument(
        "--tmux-pane",
        type=str,
        help="The tmux pane ID to send a notification to. Required unless --cancel is used."
    )
    parser.add_argument(
        "--cancel",
        action="store_true",
        help="Cancel an existing monitoring task instead of registering a new one."
    )

    args = parser.parse_args()

    payload = {
        "merge_request_id": args.mr_id,
        "project_id": args.project_id
    }

    if args.cancel:
        # Prepare a cancellation request.
        payload["command"] = "CANCEL_CI"
    else:
        # Prepare a registration request.
        if not args.tmux_pane:
            print("Error: --tmux-pane is required when registering a new task.", file=sys.stderr)
            sys.exit(2)
        payload["command"] = "REGISTER_CI"
        payload["tmux_pane_id"] = args.tmux_pane

    # Send the request and get the outcome.
    success = send_request(payload)

    # Exit with the appropriate status code.
    if success:
        sys.exit(0) # Success
    else:
        sys.exit(1) # Failure


if __name__ == "__main__":
    main()
