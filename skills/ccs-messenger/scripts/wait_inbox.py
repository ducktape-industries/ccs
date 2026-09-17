#!/usr/bin/env python3
"""Wait without consuming CCS inbox messages. Exit 0=match, 2=timeout, 1=error."""
import argparse
import json
import subprocess
import sys
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ccs", required=True)
    parser.add_argument("--session", required=True)
    parser.add_argument("--peer", required=True)
    parser.add_argument("--reply-to")
    parser.add_argument("--timeout", type=float, default=45)
    args = parser.parse_args()
    if not 0 < args.timeout <= 60:
        parser.error("--timeout must be greater than 0 and at most 60 seconds")
    deadline = time.monotonic() + args.timeout
    try:
        while time.monotonic() < deadline:
            offset = 0
            seen = set()
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    return 2
                result = subprocess.run(
                    [args.ccs, "inbox", "--session", args.session,
                     "--limit", "20", "--offset", str(offset)],
                    stdin=subprocess.DEVNULL, capture_output=True, text=True,
                    timeout=min(10, remaining), check=True,
                )
                page = json.loads(result.stdout)
                matches = [m for m in page["messages"] if m["from"] == args.peer
                           and (args.reply_to is None or m.get("reply_to") == args.reply_to)]
                if matches:
                    print(json.dumps({"messages": matches}, ensure_ascii=False))
                    return 0
                next_offset = page["next_offset"]
                if next_offset is None:
                    break
                if not isinstance(next_offset, int) or next_offset <= offset or next_offset in seen:
                    raise ValueError("invalid or repeated pagination offset")
                seen.add(next_offset)
                offset = next_offset
            time.sleep(max(0, min(3, deadline - time.monotonic())))
    except subprocess.TimeoutExpired:
        if time.monotonic() >= deadline:
            return 2
        print("CCS inbox command timed out", file=sys.stderr)
        return 1
    except subprocess.CalledProcessError as error:
        print(f"CCS inbox failed: {error.stderr.strip() or error}", file=sys.stderr)
        return 1
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"CCS inbox failed: {error}", file=sys.stderr)
        return 1
    return 2


if __name__ == "__main__":
    sys.exit(main())
