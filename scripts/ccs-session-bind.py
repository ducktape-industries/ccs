#!/usr/bin/env python3
"""Refresh a stable CCS name when a Codex or Claude session starts."""
import json
import os
from pathlib import Path
import subprocess
import sys


def claude_print_mode():
    # Claude may launch hooks through a shell, so inspect ancestors, not just the hook's parent.
    pid = os.getppid()
    for _ in range(12):
        if pid <= 1:
            break
        try:
            argv = [arg for arg in Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0') if arg]
            if argv and Path(os.fsdecode(argv[0])).name == 'claude':
                return any(arg in (b'-p', b'--print') for arg in argv[1:])
            status = Path(f'/proc/{pid}/status').read_text()
            pid = int(next(line.split()[1] for line in status.splitlines() if line.startswith('PPid:')))
        except (OSError, ValueError, StopIteration):
            break
    return False


def main():
    event = json.load(sys.stdin)
    if event.get('hook_event_name') != 'SessionStart':
        return
    if os.environ.get('CCS_NO_BIND') == '1':
        return
    provider = 'claude' if os.environ.get('CLAUDE_PROJECT_DIR') or os.environ.get('CLAUDE_CODE_MESSAGING_SOCKET') else 'codex'
    if provider == 'claude' and claude_print_mode():
        return
    path = Path(os.environ.get('CCS_BINDINGS_FILE', Path.home() / '.config/ccs/session-bindings.json'))
    bindings = json.loads(path.read_text())
    binding = bindings.get(str(Path(event['cwd']).resolve()))
    if not binding or binding['provider'] != provider:
        return
    if provider == 'claude' and not os.environ.get('CLAUDE_CODE_MESSAGING_SOCKET'):
        raise RuntimeError('Claude messaging socket is unavailable')
    env = os.environ.copy()
    if provider == 'codex':
        env['CODEX_THREAD_ID'] = event['session_id']
    args = [env.get('CCS_BIN', str(Path.home() / '.cargo/bin/ccs')), 'session', 'bind',
            binding['name'], '--' + provider]
    for key, value in binding.get('labels', {}).items():
        args.extend(['--label', f'{key}={value}'])
    if binding.get('bypass'):
        args.append('--bypass')
    result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=10)
    if result.returncode:
        raise RuntimeError(result.stderr.strip() or 'CCS session bind failed')


if __name__ == '__main__':
    try:
        main()
    except Exception as error:
        print(f'CCS session binding failed: {error}', file=sys.stderr)
        sys.exit(1)
