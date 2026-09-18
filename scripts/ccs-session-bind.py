#!/usr/bin/env python3
"""Refresh a stable CCS name when a Codex or Claude session starts."""
import json
import os
from pathlib import Path
import subprocess
import sys


def main():
    event = json.load(sys.stdin)
    if event.get('hook_event_name') != 'SessionStart':
        return
    provider = 'claude' if os.environ.get('CLAUDE_PROJECT_DIR') or os.environ.get('CLAUDE_CODE_MESSAGING_SOCKET') else 'codex'
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
