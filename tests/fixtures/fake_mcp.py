#!/usr/bin/env python3
"""Minimal stdio MCP server for Raven's offline tests."""
import json
import sys


def send(obj):
    sys.stdout.write(json.dumps(obj, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        method = msg.get("method")
        mid = msg.get("id")
        if method == "initialize":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "protocolVersion": "2025-03-26",
                        "capabilities": {"tools": {"listChanged": False}},
                        "serverInfo": {"name": "fake-mcp", "version": "0.0.1"},
                    },
                }
            )
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            send(
                {
                    "jsonrpc": "2.0",
                    "id": mid,
                    "result": {
                        "tools": [
                            {
                                "name": "echo_text",
                                "description": "Echo the given text",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {
                                        "text": {"type": "string"}
                                    },
                                    "required": ["text"],
                                },
                                "annotations": {"readOnlyHint": True},
                            },
                            {
                                "name": "boom",
                                "description": "Destructive stub",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {},
                                },
                                "annotations": {
                                    "destructiveHint": True,
                                    "readOnlyHint": False,
                                },
                            },
                            {
                                "name": "plain",
                                "description": "Unannotated stub",
                                "inputSchema": {
                                    "type": "object",
                                    "properties": {},
                                },
                            },
                        ]
                    },
                }
            )
        elif method == "tools/call":
            params = msg.get("params") or {}
            name = params.get("name")
            args = params.get("arguments") or {}
            if name == "echo_text":
                text = args.get("text") or ""
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "content": [{"type": "text", "text": text}],
                            "isError": False,
                        },
                    }
                )
            else:
                send(
                    {
                        "jsonrpc": "2.0",
                        "id": mid,
                        "result": {
                            "content": [{"type": "text", "text": "boom"}],
                            "isError": False,
                        },
                    }
                )


if __name__ == "__main__":
    main()
