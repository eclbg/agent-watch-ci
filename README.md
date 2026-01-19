# What's this?

A vibe-coded contraption so CLI-based coding agents get notified when the CI pipeline of the MR
they're working on finishes. Works with any CLI coding agent.

The tool checks whether the agent session has changed since the ci-watch command was issued. It
literally reads the contents of the terminal with tmux capture-pane. If the session has not changed,
the agent is prompted automatically.  

If the session has changed, the user gets a notification with a button to focus the agent terminal.
This only works on MacOs and you can specify your terminal with `--terminal "Ghostty"`. Defaults to
Kitty which is what I use. 

# Installation

You'll need ci_watch.py in your path. I create a symlink from `~/.local/bin/ci-watch`. With this
command:

```
ln -s $PWD/ci_watch.py ~/.local/bin/ci-watch
```

If you don't have rust and therefore cargo, you can install it with:

```
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Build the daemon with `cargo build --release`. Then run it with `./target/release/ci_monitor`. You
need GITLAB_TOKEN set. I use 1password for that, and save logs to a file, so my full command is:

```
op run -- ./target/release/ci_monitor 2> /tmp/ci_monitor.log
```

You'll also need to have [alerter](https://github.com/vjeantet/alerter) in your path. Follow the
README in that repo for instructions (it's fast).

# Usage

Run the server with `./target/release/ci_monitor`. It expects `GITLAB_TOKEN` to be set.

Add these to your agent context:

```markdown
## Source control

This repository is hosted on GitLab. It is not hosted on GitHub. Therefore, do not attempt to use
the gh command line. Instead, rely on plain git commands, and the custom tools listed in this same
dcoument

## Custom tools

You have access to some custom bash utilities that you can run when appropriate. These have been
purpose-built to make working in this project easier, so do use them when you find it helpful or
when the user prompts you to do so:
- **Get notified when CI pipeline finishes** `ci-watch --mr-id <merge-request-id> --project-id 6576720 --tmux-pane $TMUX_PANE`
    Use this tool whenever you push changes to git and there's a MR created for the branch you're
    working on. Do it right after running git push. This will ensure you get prompted when the MR
    finishes so you can analyze the results and continue with your work.

```

You must launch the agent in a tmux-controlled terminal. If you're not familiar with it, just run `tmux` in your
terminal, and then launch the agent normally. You could even close that terminal and the agent would
continue working. `tmux` rules.

# Appendix: other custom tools

My complete custom tools section in [AGENT|CLAUDE].md is the following:

```markdown
## Custom tools

You have access to some custom bash utilities that you can run when appropriate. These have been
purpose-built to make working in this project easier, so do use them when you find it helpful or
when the user prompts you to do so:
- **Check MR pipeline results** `check_mr_pipeline <branch-name>`
- **Get GitLab Issue details** `get_gitlab_issue <issue-number>`
- **Get MR feedback** `get_mr_feedback <branch-name>`
- **Get notified when CI pipeline finishes** `ci-watch --mr-id <merge-request-id> --project-id 6576720 --tmux-pane $TMUX_PANE`
    Use this tool whenever you push changes to git and there's a MR created for the branch you're
    working on. Do it right after running git push. This will ensure you get prompted when the MR
    finishes so you can analyze the results and continue with your work.
```

I don't want to spend time to make them shareable right now, but if you really want the scripts,
drop me a line. They've all been vibe-coded.
