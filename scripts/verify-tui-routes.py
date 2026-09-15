#!/usr/bin/env python3
"""Exercise the real TUI in a PTY with temporary accounts and no network/auth."""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import pty
import select
import struct
import subprocess
import tempfile
import termios
import time


class Terminal:
    def __init__(self, binary, env):
        self.fd, slave = pty.openpty()
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.process = subprocess.Popen([str(binary), "routes", "--codex"], env=env,
                                        stdin=slave, stdout=slave, stderr=slave,
                                        start_new_session=True)
        os.close(slave)
        self.output = b""
        self.consumed = 0
        self.exited = False

    def expect(self, text):
        expected = text.encode()
        deadline = time.monotonic() + 10
        while expected not in self.output[self.consumed:]:
            if time.monotonic() > deadline:
                raise AssertionError(f"Timed out waiting for {text!r}: {self.output[-1500:]!r}")
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    data = os.read(self.fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    data = b""
                if not data:
                    raise AssertionError(f"Exited before {text!r}: {self.output[-1500:]!r}")
                self.output += data
        self.consumed = self.output.index(expected, self.consumed) + len(expected)

    def send(self, text):
        # Discard already painted output so assertions observe the next interaction.
        self.consumed = len(self.output)
        os.write(self.fd, text.encode())

    def finish(self):
        self.expect("\x1b[?1049l")
        deadline = time.monotonic() + 10
        while self.process.poll() is None:
            if time.monotonic() > deadline:
                raise AssertionError("TUI did not exit after restoring the terminal")
            if select.select([self.fd], [], [], 0.1)[0]:
                try:
                    self.output += os.read(self.fd, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
        self.exited = True
        assert self.process.returncode == 0, self.process.returncode

    def close(self):
        os.close(self.fd)
        if not self.exited:
            self.process.kill()
            self.process.wait(timeout=5)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ccs"))
    binary = parser.parse_args().binary.resolve()
    with tempfile.TemporaryDirectory(prefix="ccs-tui-routes-") as directory:
        root = Path(directory).resolve()
        config, codex = root / "claude", root / "codex"
        accounts = config / "ccs" / "accounts"
        accounts.mkdir(parents=True)
        codex.mkdir()
        for slug, provider in [("a", "codex"), ("b", "codex"), ("c", "claude")]:
            account = dict(provider=provider, email=f"{slug}@example.test", uuid=slug,
                           added_at="2026-01-01T00:00:00Z",
                           oauth=dict(accessToken="unused", refreshToken="unused", expiresAt=0))
            (accounts / f"{slug}.json").write_text(json.dumps(account))
        # No Codex client metadata: the catalog fails before any HTTP request.
        state = config / "ccs" / "state.json"
        state.write_text('{"active":"c","codex":"a"}')
        state_before = state.read_bytes()
        route_file = config / "ccs" / "routing.json"
        original = dict(provider="claude", model="existing-*", accounts=["c"])
        route_file.write_text(json.dumps(dict(rules=[original])))
        env = dict(os.environ, CLAUDE_CONFIG_DIR=str(config), CODEX_HOME=str(codex))

        terminal = Terminal(binary, env)
        try:
            terminal.expect("Model lookup failed")
            terminal.send("i")
            terminal.expect("Exact model ID")
            terminal.send("future-model-v99\r")
            terminal.expect("space toggle")
            terminal.send("\x1b[B ")
            terminal.expect("[1] b@example.test")
            terminal.send("\x1b[A ")
            terminal.expect("[2] a@example.test")
            terminal.send("\r")
            terminal.finish()
        finally:
            terminal.close()
        expected = dict(provider="codex", model="future-model-v99", accounts=["b", "a"])
        assert json.loads(route_file.read_text())["rules"] == [original, expected]
        assert state.read_bytes() == state_before

        # Reopen a saved ID while offline, change a draft, then cancel without writing.
        before = route_file.read_bytes()
        terminal = Terminal(binary, env)
        try:
            terminal.expect("future-model-v99  [saved]")
            terminal.send("\r")
            terminal.expect("[1] b@example.test")
            terminal.send(" ")
            terminal.expect("[ ] a@example.test")
            terminal.send("\x03")
            terminal.finish()
        finally:
            terminal.close()
        assert route_file.read_bytes() == before
        assert state.read_bytes() == state_before
        assert not (codex / "auth.json").exists()
        print("PTY PASS: custom ID, primary/fallback order, save, offline reopen, cancel, terminal restore")
        print("STATE PASS: other provider route and active-account state preserved; no login installed")


if __name__ == "__main__":
    main()
