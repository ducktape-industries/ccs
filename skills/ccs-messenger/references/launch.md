# Launching a short CCS conversation

Check installed `claude --help` and `codex exec --help` for supported flags. Resolve the requested model from local CLI/catalog data without substituting another family. For example, `sonnet` resolved to Sonnet 5 and `gpt-5.6-luna` was the Luna model in a verified run; inspect actual startup metadata each time.

Codex's local `$CODEX_HOME/models_cache.json` (default `~/.codex/models_cache.json`) contains model slugs when available. Confirm the launched model from its startup/session metadata, not its self-description. Claude startup output in `claude logs <id>` shows the resolved model. Record missing verification explicitly.

## Process boundary

Launch with an argv list. If a Python heredoc or another wrapper starts a CLI, explicitly use `stdin=subprocess.DEVNULL`: Claude and Codex can append inherited stdin to the user prompt, including the wrapper's Python source. Send interactive input only through a deliberate PTY or streaming protocol.

Clear inherited `CODEX_THREAD_ID`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDECODE`, and `CCS_SESSION` in each child's environment. Preserve authentication homes, PATH, and the intended `CCS_SERVER_DIR`. Registration must run inside the child so its own transport identity is detected.

```python
import os
import subprocess

def start(argv, cwd, log_path):
    env = os.environ.copy()
    for key in ("CODEX_THREAD_ID", "CLAUDE_CODE_MESSAGING_SOCKET", "CLAUDECODE", "CCS_SESSION"):
        env.pop(key, None)
    with open(log_path, "x", encoding="utf-8") as log:
        return subprocess.Popen(
            argv, cwd=cwd, env=env, stdin=subprocess.DEVNULL,
            stdout=log, stderr=subprocess.STDOUT, start_new_session=True,
        )
```

Use unique log paths in a private temporary directory. Keep the returned PID/process handle and provider session/thread ID. Launcher acceptance does not prove registration or a working model.

## Runtime choice

- Claude needs a real messaging socket. A supported `claude --bg --model sonnet --name <display-name> ... -- <prompt>` creates a background session. Confirm with `claude agents --json`; `claude logs <id>` and `claude attach <id>` inspect it. `--safe-mode` is useful for a chat-only demo that needs no customizations, but do not promise all permission flags will survive background dispatch.
- Codex can use `codex exec -m gpt-5.6-luna --json <prompt>` for a bounded conversation, provided the initial task keeps it actively polling until the exchange ends. It exits after that task. For sessions that should remain available, use an interactive PTY or supported persistent runtime instead. Do not claim a completed `exec` process will automatically wake on later messages.
- Use execution permissions appropriate to the authorized CCS commands. Do not default to `--yolo` or global permission bypass. Claude can allow the exact CCS executable and the inbox helper; unexpected shell prefixes/compound commands may still prompt. Inspect each prompt and approve only authorized operations; do not blindly auto-confirm terminal text. For a Claude actually running in bypass mode, register with `--bypass` to attest that mode; never add this flag for Codex.
- Background launch configuration can vary by CLI version. Check actual model, permission state, and environment from the running child. If setup fails, correct that session rather than launching duplicates.

## Child prompt contract

Give each child only its role. Include the actual user request, executable/helper paths, own name, peer name, topic, overall deadline, and number of exchanges. Quote path/message arguments safely. Include:

> You are the already-running participant, not the launcher. Do not create more agents. Register THIS session using the specified provider flag and `--label role=worker` for a bounded worker. Execute only the CCS conversation task. Use --session YOUR_NAME for every message command. Read actual incoming text and compose your own reply. Keep messages short. Do not perform code changes, account switching or unrelated integrations. After the specified exchanges, run `ccs session remove YOUR_NAME`; the message history remains.

Start the responder first; wait for its registration before starting/sending from the initiator.

**Responder:** register; use the helper to wait for messages from the initiator; read each message; `reply` to its exact ID. After the agreed count, report the IDs and finish the task. Empty polls may be repeated only within the overall deadline.

**Initiator:** register; `inbox send` the opening; save the returned ID; use the helper with `--peer` and `--reply-to` for that ID; read and `ack` the reply. Compose the next question from that actual answer. Repeat to the agreed count and finish. Send each question only once.

The launcher checks `ccs sessions` after each child exits and removes any temporary registration that child could not release. Do not remove unrelated or long-lived sessions.

Use asynchronous inbox messages for this polling demo. To exercise queue delivery instead, keep the recipient runtime alive, use one synchronous initiator, and have the recipient explicitly `reply`. Never simulate either model's answer in the orchestrator.
