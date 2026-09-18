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
        assert m['receipt']['stored'] is True and m['receipt']['read'] is False
        assert m['receipt']['unread_for_ms'] >= 0
        assert m['id'] in (root / 'delivered').read_text(), 'inbox must wake recipient'
        assert run('message', m['id'])['receipt']['unread_for_ms'] >= 0
        first_read = run('inbox', session='bob')['messages'][0]
        assert first_read['id'] == m['id'] and first_read['status'] == 'read'
        assert run('inbox', session='bob')['messages'][0]['id'] == m['id'], 'read must not consume'
        assert run('message', m['id'])['receipt']['read'] is True
        run('reply', m['id'], '--message', 'wrong recipient', ok=False)
        handled = run('inbox', 'ack', m['id'], session='bob')
        assert handled['status'] == 'handled' and handled['receipt']['read'] is True
        assert run('inbox', session='bob')['messages'] == []
        question = run('inbox', 'send', 'bob', '--message', 'async question')
        (root / 'delivered').unlink()
        run('reply', question['id'], '--message', 'async answer', session='bob')
        assert ('r' + question['id']) in (root / 'delivered').read_text(), 'reply inbox must wake sender'
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
        read_only = run('inbox', 'send', 'bob', '--message', 'read state survives restart')
        assert any(m['id'] == read_only['id'] and m['status'] == 'read' for m in run('inbox', session='bob')['messages'])
        # Restart retains sessions and messages; a second daemon must not steal the socket.
        second = subprocess.run([binary, 'server'], env=base, capture_output=True, timeout=5)
        assert second.returncode != 0
        server.terminate(); server.wait(timeout=5)
        server = start()
        assert run('message', m['id'])['status'] == 'handled'
        assert run('message', read_only['id'])['status'] == 'read'
        run('inbox', 'ack', read_only['id'], session='bob')
        # A pre-upgrade "read" meant acknowledged, so migrate it to handled once.
        server.terminate(); server.wait(timeout=5)
        old_state = json.loads((root / 'bus/state.json').read_text())
        old_state.pop('schema_version')
        old_state['messages'][m['id']]['status'] = 'read'
        (root / 'bus/state.json').write_text(json.dumps(old_state))
        server = start()
        assert run('message', m['id'])['status'] == 'handled'
        payload = 'What type? $(touch must-not-exist) `literal`\nsecond line'
        (root / 'delivered').unlink()
        waiting = subprocess.Popen([binary, 'queue', 'bob', '--message', payload, '--timeout', '5'], env=env(), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        for _ in range(100):
            if (root / 'delivered').exists(): break
            time.sleep(.03)
        delivered = (root / 'delivered').read_text()
        assert payload in delivered and 'bob-thread' in delivered
        assert 'CCS_SERVER_DIR=' not in delivered and 'ccs reply' not in delivered
        assert 'Reply via' not in delivered and 'Reply using' not in delivered
        pending = run('inbox', session='bob')['messages']
        q = next(x for x in pending if x['kind'] == 'queue')
        assert f"From: alice\nTo: bob\nMessage-ID: {q['id']}\n" in delivered
        run('inbox', 'ack', q['id'], session='bob', ok=False)
        run('reply', q['id'], '--message', 'string', session='bob')
        stdout, stderr = waiting.communicate(timeout=8)
        assert waiting.returncode == 0, stderr
        assert json.loads(stdout)['reply'] == 'string'
        assert json.loads(stdout)['receipt'] == {'stored': True, 'delivered': True, 'read': True}
        run('reply', q['id'], '--message', 'overwrite', session='bob', ok=False)
        err = run('queue', 'bob', '--message', 'unanswered', '--timeout', '1', ok=False)
        assert 'timed out' in err and 'message' in err
        assert '"stored":true' in err and '"delivered":null' in err
        (root / 'reject').touch()
        err = run('queue', 'bob', '--message', 'transport failed', '--timeout', '2', ok=False)
        assert 'failed' in err
        assert '"stored":true' in err and '"delivered":null' in err and '"read":false' in err
        (root / 'reject').unlink()
        run('session', 'register', 'bob', '--codex', ok=False)  # alice cannot silently rebind bob
        run('session', 'bind', 'bob', '--codex', session='bob-new')
        state = json.loads((root / 'bus/state.json').read_text())
        assert state['sessions']['bob']['endpoint']['thread'] == 'bob-new-thread'
        assert state['sessions']['bob']['labels']['repo'] == 'web'
        bindings = root / 'bindings.json'
        bindings.write_text(json.dumps({str(root): {'name':'bob','provider':'codex'}}))
        hook_env = dict(base, CCS_BIN=binary, CCS_BINDINGS_FILE=str(bindings))
        hook = subprocess.run([sys.executable, str(Path(__file__).with_name('ccs-session-bind.py'))],
            input=json.dumps({'hook_event_name':'SessionStart','cwd':str(root),'session_id':'hook-thread'}),
            env=hook_env, text=True, capture_output=True, timeout=10)
        assert hook.returncode == 0, hook.stderr
        assert json.loads((root / 'bus/state.json').read_text())['sessions']['bob']['endpoint']['thread'] == 'hook-thread'
        rebound = run('inbox', 'send', 'bob', '--message', 'after Codex rebind')
        assert rebound['status'] == 'pending'
        probe = run('queue', 'bob', '--message', 'Codex rebind works', '--timeout', '1', ok=False)
        assert 'timed out' in probe and 'hook-thread' in (root / 'delivered').read_text(), (probe, (root / 'delivered').read_text())
        # A dropped initial connection leaves a recoverable, caller-known ID.
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
        registration = subprocess.run([binary, 'session', 'bind', 'bob', '--claude'], env=claude_env, capture_output=True)
        assert registration.returncode != 0, 'binding cannot change a name to another provider'
        registration = subprocess.run([binary, 'session', 'register', 'carol', '--claude', '--bypass', '--label', 'role=worker'], env=claude_env, capture_output=True)
        assert registration.returncode == 0, registration.stderr
        second_socket = root / '1000.sock'
        second_listener = socket.socket(socket.AF_UNIX)
        second_listener.bind(str(second_socket)); second_listener.listen()
        (config / 'sessions/1000.test.key').write_text(json.dumps({'peerToken':'fixture-token'}))
        rebound_env = dict(claude_env, CLAUDE_CODE_MESSAGING_SOCKET=str(second_socket))
        registration = subprocess.run([binary, 'session', 'bind', 'carol', '--claude', '--bypass'], env=rebound_env, capture_output=True)
        assert registration.returncode == 0, registration.stderr
        state = json.loads((root / 'bus/state.json').read_text())
        assert state['sessions']['carol']['endpoint']['socket'] == str(second_socket)
        assert state['sessions']['carol']['labels']['role'] == 'worker'
        bindings.write_text(json.dumps({str(root): {'name':'carol','provider':'claude','bypass':True}}))
        hook = subprocess.run([sys.executable, str(Path(__file__).with_name('ccs-session-bind.py'))],
            input=json.dumps({'hook_event_name':'SessionStart','cwd':str(root),'session_id':'carol-session'}),
            env=dict(rebound_env, CCS_BIN=binary, CCS_BINDINGS_FILE=str(bindings)),
            text=True, capture_output=True, timeout=10)
        assert hook.returncode == 0, hook.stderr
        listener.close(); listener = second_listener
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
                    mid = text.split('Message-ID: ', 1)[1].splitlines()[0]
                    assert 'CCS_SERVER_DIR=' not in text and 'ccs reply' not in text
                    assert 'Reply via' not in text and 'Reply using' not in text
                reply_args = {'op':'reply', 'id':mid, 'session':'carol', 'text':'Claude answer'}
                frames = [
                    {'jsonrpc':'2.0', 'id':1, 'method':'initialize', 'params':{'protocolVersion':'2025-11-25'}},
                    {'jsonrpc':'2.0', 'id':2, 'method':'tools/call', 'params':{'name':'ccs', 'arguments':reply_args}},
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
        room = query(dict(op='room_history', kind=None, limit=100, offset=0))
        assert any(x['id'] == literal['id'] for x in room['messages'])
        assert len({x['id'] for x in room['messages']}) == len(room['messages'])
        assert room['messages'][0]['sequence'] > room['messages'][-1]['sequence']
        user_message = query(dict(op='post', id='user-to-bob', to='bob', body='Please review this'))
        assert user_message['from'] == 'ccs-user' and user_message['status'] == 'submitted'
        assert 'Please review this' in (root / 'delivered').read_text()
        assert query(dict(op='message', session='ccs-user', id='user-to-bob'))['id'] == 'user-to-bob'
        sessions = run('sessions')
        assert next(x for x in sessions if x['name'] == 'carol')['provider'] == 'claude'
        assert next(x for x in sessions if x['name'] == 'bob')['last_activity'] > 0
        registration = subprocess.run([binary, 'session', 'register', 'dave', '--claude', '--label', 'role=manager'],
            env=rebound_env, capture_output=True)
        assert registration.returncode == 0, registration.stderr
        second_socket.unlink()
        failed = run('queue', 'dave', '--message', 'dead socket must fail', '--timeout', '1', ok=False)
        assert 'target-endpoint-dead' in failed
        assert '"stored":true' in failed and '"delivered":false' in failed
        assert any(m['body'] == 'dead socket must fail' and m['status'] == 'failed'
                   for m in run('inbox', session='dave')['messages'])
        assert any(m['body'] == 'dead socket must fail' and m['receipt'] == {'stored': True, 'delivered': False, 'read': False}
                   for m in run('inbox', session='dave')['messages'])
        unwoken = run('inbox', 'send', 'dave', '--message', 'wake failed but message is stored')
        assert unwoken['status'] == 'pending' and 'target-endpoint-dead' in unwoken['error']
        assert unwoken['receipt']['unread_for_ms'] >= 0
        assert any(m['id'] == unwoken['id'] and m['status'] == 'read' for m in run('inbox', session='dave')['messages'])
        assert not any(x['name'] == 'carol' for x in run('sessions'))
        assert query(dict(op='message', session='alice', id=answered['id']))['reply'] == 'Claude answer'

        run('session', 'remove', 'bob')
        assert run('sessions', '--label', 'repo=web') == []
        assert not (root / 'must-not-exist').exists()
        assert (root / 'bus').stat().st_mode & 0o777 == 0o700
        assert (root / 'bus/state.json').stat().st_mode & 0o777 == 0o600
        print('messenger: labels, inbox, persistence, replies, timeout, transport failure, permissions passed')
    finally:
        server.terminate()
        server.wait(timeout=5)
