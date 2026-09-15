#!/usr/bin/env python3
"""Prove installed Claude Code sends Fable + Opus subagents to one local endpoint.

No account credentials or real inference: a temporary configuration and a local
Anthropic SSE stub drive one Agent tool call. Requires `claude` on PATH.
"""
import http.server
import json
import os
import subprocess
import tempfile
import threading

seen = []
lock = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_args):
        pass

    def do_GET(self):
        raw = b'{"data":[],"has_more":false}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def do_POST(self):
        data = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))))
        model = data.get("model", "")
        with lock:
            first_parent = "fable" in model and not any("fable" in r["model"] for r in seen)
            seen.append({"model": model, "local_auth": self.headers.get("Authorization") == "Bearer sk-ant-oat-ccs-test", "oauth_beta": "oauth-2025-04-20" in self.headers.get("anthropic-beta", "")})
        tool = {"type": "tool_use", "id": "toolu_local_probe", "name": "Agent", "input": {"description": "Check local subagent routing", "prompt": "Reply LOCAL_SUBAGENT_OK. Do not use any tools.", "subagent_type": "general-purpose", "model": "opus", "run_in_background": False}}
        content = tool if first_parent else {"type": "text", "text": "LOCAL_SUBAGENT_OK" if "opus" in model else "LOCAL_ROUTING_OK"}
        reason = "tool_use" if first_parent else "end_turn"
        message = {"id": "msg_local_probe", "type": "message", "role": "assistant", "model": model, "content": [content], "stop_reason": reason, "stop_sequence": None, "usage": {"input_tokens": 10, "output_tokens": 5}}
        if not data.get("stream"):
            raw = json.dumps(message).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        block = dict(tool, input={}) if first_parent else {"type": "text", "text": ""}
        delta = {"type": "input_json_delta", "partial_json": json.dumps(tool["input"])} if first_parent else {"type": "text_delta", "text": content["text"]}
        events = [
            ("message_start", {"message": dict(message, content=[], stop_reason=None)}),
            ("content_block_start", {"index": 0, "content_block": block}),
            ("content_block_delta", {"index": 0, "delta": delta}),
            ("content_block_stop", {"index": 0}),
            ("message_delta", {"delta": {"stop_reason": reason, "stop_sequence": None}, "usage": {"output_tokens": 5}}),
            ("message_stop", {}),
        ]
        for name, event in events:
            self.wfile.write(f"event: {name}\ndata: {json.dumps(dict(event, type=name))}\n\n".encode())


def main():
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        with tempfile.TemporaryDirectory(prefix="ccs-claude-routing-") as config:
            env = dict(os.environ, CLAUDE_CONFIG_DIR=config, ANTHROPIC_BASE_URL=f"http://127.0.0.1:{server.server_port}", CLAUDE_CODE_OAUTH_TOKEN="sk-ant-oat-ccs-test", CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC="1")
            for key in ["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_REFRESH_TOKEN", "CLAUDECODE", "CLAUDE_CODE_USE_BEDROCK", "CLAUDE_CODE_USE_VERTEX", "CLAUDE_CODE_USE_FOUNDRY"]:
                env.pop(key, None)
            result = subprocess.run([os.environ.get("CCS_CLAUDE_BINARY", "claude"), "-p", "Use an Opus subagent to reply LOCAL_SUBAGENT_OK, then reply LOCAL_ROUTING_OK.", "--model", "fable", "--max-turns", "4", "--allowedTools", "Agent", "--settings", '{"disableAllHooks":true}', "--setting-sources", ""], env=env, cwd=config, capture_output=True, text=True, timeout=60)
            print(json.dumps({"returncode": result.returncode, "requests": seen, "result": result.stdout.strip(), "stderr": result.stderr.strip()}, indent=2))
            assert result.returncode == 0, "Claude Code failed"
            assert any("fable" in r["model"] for r in seen), "missing parent request"
            assert any("opus" in r["model"] for r in seen), "missing Opus subagent request"
            assert all(r["local_auth"] and r["oauth_beta"] for r in seen), "auth not inherited"
            assert "LOCAL_ROUTING_OK" in result.stdout, "parent did not finish"
    finally:
        server.shutdown()


if __name__ == "__main__":
    main()
