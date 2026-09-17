#!/usr/bin/env python3
"""Exercise real CCS server/CLI boundaries with isolated homes and a fake Codex transport."""
import json
import os
import select
import socket
import threading
from pathlib import Path
import subprocess
import sys
import tempfile
import time

binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else 'target/debug/ccs').resolve())
with tempfile.TemporaryDirectory(prefix='ccs-msg-') as tmp:
    root = Path(tmp)
    codex = root / 'codex'
    codex.write_text('#!/bin/sh\nif [ "$1" = "queue" ] && [ "$2" = "--help" ]; then echo "--thread --message"; exit; fi\nif [ -f "$HOME/reject" ]; then exit 1; fi\nprintf "%s\\n" "$@" > "$HOME/delivered"\n')
    codex.chmod(0o700)
    (root / '.codex').mkdir()
    base = dict(os.environ, HOME=tmp, CCS_SERVER_DIR=str(root / 'bus'), CCS_CODEX_BINARY=str(codex))
    base.pop('CLAUDE_CODE_MESSAGING_SOCKET', None)
    base.pop('CCS_SESSION', None)
    def env(session='alice'):
        return dict(base, CCS_SESSION=session, CODEX_THREAD_ID=session + '-thread')
    def run(*args, session='alice', ok=True):
        result = subprocess.run([binary, *args], env=env(session), capture_output=True, text=True, timeout=10)
        assert (result.returncode == 0) == ok, (args, result.stdout, result.stderr)
        return json.loads(result.stdout) if ok else result.stderr
    def start():
        server = subprocess.Popen([binary, 'server'], env=base, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        for _ in range(100):
            if select.select([server.stderr], [], [], 0)[0]:
                line = server.stderr.readline()
                assert 'listening on' in line, line
                return server
            if server.poll() is not None:
                raise AssertionError(('server failed to start', server.communicate()))
            time.sleep(.03)
        server.kill()
        raise AssertionError('server startup timeout')
    server = start()
    try:
        run('session', 'register', 'alice', '--adapter', 'codex', '--label', 'role=manager')
        assert 'unknown adapter' in run('session', 'register', 'unknown', '--adapter', 'unimplemented', ok=False)
        assert 'choose one' in run('session', 'register', 'ambiguous', '--adapter', 'codex', '--claude', ok=False)
        run('session', 'register', 'bob', '--codex', '--label', 'role=manager', '--label', 'repo=ui', session='bob')
        assert len(run('sessions', '--label', 'role=manager')) == 2
        assert run('sessions', '--label', 'repo=ui')[0]['name'] == 'bob'
        run('session', 'label', 'bob', '--label', 'repo=web')
        assert run('sessions', '--label', 'repo=ui') == []
        assert 'exactly one' in run('inbox', 'send', '--label', 'role=manager', '--message', 'ambiguous', ok=False)
        assert 'exactly one' in run('inbox', 'send', 'missing', '--message', 'missing', ok=False)
        m = run('inbox', 'send', '--label', 'repo=web', '--message', 'review later')
        assert not (root / 'delivered').exists(), 'inbox must not push'
        assert run('inbox', session='bob')['messages'][0]['id'] == m['id']
        assert run('inbox', session='bob')['messages'][0]['id'] == m['id'], 'read must not consume'
        run('reply', m['id'], '--message', 'wrong recipient', ok=False)
        run('inbox', 'ack', m['id'], session='bob')
        assert run('inbox', session='bob')['messages'] == []
        question = run('inbox', 'send', 'bob', '--message', 'async question')
        run('reply', question['id'], '--message', 'async answer', session='bob')
        answer = run('inbox')['messages'][0]
        assert answer['reply_to'] == question['id'] and answer['body'] == 'async answer'
        run('inbox', 'ack', answer['id'])
        # Large inboxes remain accessible page by page.
        ids = []
        for _ in range(18):
            ids.append(run('inbox', 'send', 'bob', '--message', 'x' * 65536)['id'])
        seen, offset = [], 0
        while True:
            page = run('inbox', '--limit', '100', '--offset', str(offset), session='bob')
            seen.extend(x['id'] for x in page['messages'])
            if page['next_offset'] is None: break
            offset = page['next_offset']
        assert set(seen) == set(ids) and len(seen) == len(ids)
        for mid in ids: run('inbox', 'ack', mid, session='bob')
        # Restart retains sessions and messages; a second daemon must not steal the socket.
        second = subprocess.run([binary, 'server'], env=base, capture_output=True, timeout=5)
        assert second.returncode != 0
        server.terminate(); server.wait(timeout=5)
        server = start()
        assert run('message', m['id'])['status'] == 'read'
        payload = 'What type? $(touch must-not-exist) `literal`\nsecond line'
        waiting = subprocess.Popen([binary, 'queue', 'bob', '--message', payload, '--timeout', '5'], env=env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        for _ in range(100):
            if (root / 'delivered').exists(): break
            time.sleep(.03)
        delivered = (root / 'delivered').read_text()
        assert payload in delivered and 'bob-thread' in delivered
        assert 'CCS_SERVER_DIR=' not in delivered and 'ccs reply' not in delivered
        reply_args = json.loads(delivered.rsplit('Reply via MCP: ccs(', 1)[1].strip()[:-1])
        assert reply_args['op'] == 'reply' and reply_args['session'] == 'bob'
        pending = run('inbox', session='bob')['messages']
        q = next(x for x in pending if x['kind'] == 'queue')
        assert reply_args['id'] == q['id']
        run('inbox', 'ack', q['id'], session='bob', ok=False)
        run('reply', q['id'], '--message', 'string', session='bob')
        stdout, stderr = waiting.communicate(timeout=8)
        assert waiting.returncode == 0, stderr
        assert json.loads(stdout)['reply'] == 'string'
        run('reply', q['id'], '--message', 'overwrite', session='bob', ok=False)
        err = run('queue', 'bob', '--message', 'unanswered', '--timeout', '1', ok=False)
        assert 'timed out' in err and 'message' in err
        (root / 'reject').touch()
        err = run('queue', 'bob', '--message', 'transport failed', '--timeout', '2', ok=False)
        assert 'failed' in err
        run('session', 'register', 'bob', '--codex', ok=False)  # alice cannot silently rebind bob
        # A dropped initial connection leaves a recoverable, caller-known ID.
        (root / 'reject').unlink()
        wire = socket.socket(socket.AF_UNIX)
        wire.connect(str(root / 'bus/server.sock'))
        wire.sendall((json.dumps(dict(op='send', id='lost-response', **{'from': 'alice'}, to='bob', labels={}, kind='queue', body='recover me')) + '\n').encode())
        wire.close()
        for _ in range(100):
            result = subprocess.run([binary, 'message', 'lost-response'], env=env(), capture_output=True, text=True)
            if result.returncode == 0 and json.loads(result.stdout)['status'] == 'submitted': break
            time.sleep(.03)
        assert result.returncode == 0 and json.loads(result.stdout)['body'] == 'recover me'
        # Messages that resemble options remain literal text.
        literal = run('inbox', 'send', 'bob', '--message', '--help')
        assert literal['body'] == '--help'
        # Claude adapter: authentication, bypass attestation, and a real correlated reply.
        config = root / '.claude'
        (config / 'sessions').mkdir(parents=True)
        sockpath = root / '999.sock'
        listener = socket.socket(socket.AF_UNIX)
        listener.bind(str(sockpath)); listener.listen()
        (config / 'sessions/999.test.key').write_text(json.dumps({'peerToken':'fixture-token'}))
        claude_env = dict(env('carol'), CLAUDE_CODE_MESSAGING_SOCKET=str(sockpath), CLAUDE_CONFIG_DIR=str(config))
        registration = subprocess.run([binary, 'session', 'register', 'bob', '--claude'], env=claude_env, capture_output=True)
        assert registration.returncode != 0, 'Claude must not take the existing Codex name'
        registration = subprocess.run([binary, 'session', 'register', 'carol', '--claude', '--bypass'], env=claude_env, capture_output=True)
        assert registration.returncode == 0, registration.stderr
        failures = []
        def receive_claude():
            try:
                conn, _ = listener.accept()
                conn.settimeout(5)
                with conn, conn.makefile('rb') as incoming:
                    auth = json.loads(incoming.readline())
                    message = json.loads(incoming.readline())
                    assert auth == {'type': 'auth', 'token': 'fixture-token'}
                    assert message['from_mode'] == 'bypass'
                    assert message['priority'] == 'next'
                    text = message['message']['content']
                    mid = text.split('CCS request ', 1)[1].split()[0]
                    assert 'CCS_SERVER_DIR=' not in text and 'ccs reply' not in text
                    guide = json.loads(text.rsplit('Reply via MCP: ccs(', 1)[1].splitlines()[0][:-1])
                    assert guide['id'] == mid and guide['session'] == 'carol'
                guide['text'] = 'Claude answer'
                frames = [
                    {'jsonrpc':'2.0', 'id':1, 'method':'initialize', 'params':{'protocolVersion':'2025-11-25'}},
                    {'jsonrpc':'2.0', 'id':2, 'method':'tools/call', 'params':{'name':'ccs', 'arguments':guide}},
                ]
                reply = subprocess.run([binary, 'mcp'], env=claude_env,
                    input=''.join(json.dumps(frame) + '\n' for frame in frames),
                    capture_output=True, text=True, timeout=5)
                assert reply.returncode == 0, reply.stderr
                result = json.loads(reply.stdout.splitlines()[-1])['result']
                assert not result['isError'], result
            except BaseException as error:
                failures.append(error)
        receiver = threading.Thread(target=receive_claude, daemon=True)
        receiver.start()
        answered = run('queue', 'carol', '--message', 'Codex to Claude', '--timeout', '5')
        receiver.join(timeout=5); listener.close()
        assert not receiver.is_alive() and not failures, failures
        assert answered['reply'] == 'Claude answer'
        # UI history includes outgoing queue outcomes, and filters by transport kind.
        def query(request):
            with socket.socket(socket.AF_UNIX) as wire:
                wire.connect(str(root / 'bus/server.sock'))
                wire.sendall((json.dumps(request) + '\n').encode())
                with wire.makefile('r') as incoming:
                    result = json.loads(incoming.readline())
            assert 'error' not in result, result
            return result['ok']
        history = query(dict(op='history', session='alice', kind='queue', limit=100, offset=0))
        assert any(x['status'] == 'answered' for x in history['messages'])
        assert any(x['status'] == 'failed' for x in history['messages'])
        assert all(x['kind'] == 'queue' for x in history['messages'])
        assert history['messages'][0]['sequence'] > history['messages'][-1]['sequence']
        sessions = run('sessions')
        assert next(x for x in sessions if x['name'] == 'carol')['provider'] == 'claude'

        run('session', 'remove', 'bob')
        assert run('sessions', '--label', 'repo=web') == []
        assert not (root / 'must-not-exist').exists()
        assert (root / 'bus').stat().st_mode & 0o777 == 0o700
        assert (root / 'bus/state.json').stat().st_mode & 0o777 == 0o600
        print('messenger: labels, inbox, persistence, replies, timeout, transport failure, permissions passed')
    finally:
        server.terminate()
        server.wait(timeout=5)
