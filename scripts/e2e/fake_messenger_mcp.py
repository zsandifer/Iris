#!/usr/bin/env python3
"""A harmless fake "messenger" MCP server for testing Iris's connector
gateway with real CLIs. Nothing is ever sent anywhere: every call is
appended to a JSON-lines log so a test can check exactly what ran.

Tools: search_messages (read), send_message (send), delete_message
(delete), read_archive (returns a lot of text; for the bulk-read
tripwire).

Environment:
  FAKE_MESSENGER_TOKEN  required "secret" (proves Iris injects connector
                        secrets on its side; the agent never sees it)
  FAKE_MESSENGER_LOG    log file (default: ./messenger-calls.jsonl)

Standard library only. Speaks MCP over stdio (one JSON-RPC message per
line).
"""
import json
import os
import sys
import time

LOG = os.environ.get("FAKE_MESSENGER_LOG", "messenger-calls.jsonl")
TOKEN = os.environ.get("FAKE_MESSENGER_TOKEN", "")

TOOLS = [
    {
        "name": "search_messages",
        "description": "Search the user's messages.",
        "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}},
        "annotations": {"readOnlyHint": True},
    },
    {
        "name": "send_message",
        "description": "Send a text message to one recipient (an email address or phone number).",
        "inputSchema": {
            "type": "object",
            "properties": {"to": {"type": "string"}, "text": {"type": "string"}},
            "required": ["to", "text"],
        },
    },
    {
        "name": "delete_message",
        "description": "Delete a message by id.",
        "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]},
    },
    {
        "name": "read_archive",
        "description": "Read one page of the full message archive (large).",
        "inputSchema": {"type": "object", "properties": {"page": {"type": "integer"}}},
        "annotations": {"readOnlyHint": True},
    },
]


def log(entry):
    entry["at"] = time.time()
    with open(LOG, "a", encoding="utf-8") as f:
        f.write(json.dumps(entry) + "\n")


def text(t, error=False):
    out = {"content": [{"type": "text", "text": t}]}
    if error:
        out["isError"] = True
    return out


def call(name, args):
    if not TOKEN:
        return text("unauthorized: no messenger token", error=True)
    log({"tool": name, "arguments": args})
    if name == "search_messages":
        return text(json.dumps([
            {"id": "m1", "from": "sam@example.com", "text": "Are we still on for dinner?"},
            {"id": "m2", "from": "kim@example.com", "text": "Package arrives Tuesday."},
        ]))
    if name == "send_message":
        return text(f"Sent to {args.get('to', '?')}.")
    if name == "delete_message":
        return text(f"Deleted {args.get('id', '?')}.")
    if name == "read_archive":
        page = int(args.get("page", 1) or 1)
        line = f"Archive page {page}: an old message about nothing in particular. "
        return text(line * 6000)
    return text(f"unknown tool {name}", error=True)


def main():
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
        except ValueError:
            continue
        if "id" not in req:
            continue
        method = req.get("method")
        if method == "initialize":
            result = {
                "protocolVersion": req.get("params", {}).get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake-messenger", "version": "1"},
            }
        elif method == "tools/list":
            result = {"tools": TOOLS}
        elif method == "tools/call":
            p = req.get("params", {})
            result = call(p.get("name", ""), p.get("arguments") or {})
        elif method == "ping":
            result = {}
        else:
            reply = {"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32601, "message": "not found"}}
            print(json.dumps(reply), flush=True)
            continue
        print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)


if __name__ == "__main__":
    main()
