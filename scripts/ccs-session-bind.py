#!/usr/bin/env python3
"""Refresh a stable CCS name when a Codex or Claude session starts."""
import json
import os
from pathlib import Path
import subprocess
import sys


def proc_argv_and_parent(pid):
    """A process's exact argv and its parent, from /proc where there is one."""
    if Path('/proc/self').exists():
        argv = [arg for arg in Path(f'/proc/{pid}/cmdline').read_bytes().split(b'\0') if arg]
        status = Path(f'/proc/{pid}/status').read_text()
        return argv, int(next(line.split()[1] for line in status.splitlines() if line.startswith('PPid:')))
    # macOS has no /proc: KERN_PROCARGS2 holds argc, the exec path, then argv.
    import ctypes
    import struct
    libc = ctypes.CDLL(None, use_errno=True)
    mib = (ctypes.c_int * 3)(1, 49, pid)  # CTL_KERN, KERN_PROCARGS2
    size = ctypes.c_size_t(0)
    if libc.sysctl(mib, 3, None, ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), f'sysctl size for {pid}')
    buf = ctypes.create_string_buffer(size.value)
    if libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0):
        raise OSError(ctypes.get_errno(), f'sysctl args for {pid}')
    raw = buf.raw[:size.value]
    argc = struct.unpack('i', raw[:4])[0]
    rest = raw[4:].split(b'\0', 1)[1].lstrip(b'\0')  # skip the exec path and its padding
    argv = rest.split(b'\0')[:argc]
    parent = subprocess.run(['ps', '-o', 'ppid=', '-p', str(pid)], text=True, capture_output=True)
    return argv, int(parent.stdout)


def claude_print_mode():
    # Claude may launch hooks through a shell, so inspect ancestors, not just the hook's parent.
    pid = os.getppid()
    for _ in range(12):
        if pid <= 1:
            break
        try:
            argv, parent = proc_argv_and_parent(pid)
            if argv and Path(os.fsdecode(argv[0])).name == 'claude':
                return any(arg in (b'-p', b'--print') for arg in argv[1:])
            pid = parent
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
