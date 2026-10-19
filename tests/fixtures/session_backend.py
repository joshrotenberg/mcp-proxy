"""Strict initialized stdio MCP peer. The call journal proves denied calls stay out."""
import json
import sys

initialized = False
for line in sys.stdin:
    request = json.loads(line)
    method = request["method"]
    if method == "notifications/initialized":
        initialized = True
    if "id" not in request:
        continue
    error = None
    if method == "initialize":
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "session-fixture", "version": "1"}}
    elif not initialized:
        error = {"code": -32600, "message": "initialized notification required"}
    elif method == "tools/list":
        result = {"tools": [{"name": name, "inputSchema": {"type": "object"}}
                            for name in ["echo", "blocked"]]}
    elif method == "tools/call":
        with open(sys.argv[1], "a", encoding="utf-8") as journal:
            journal.write(json.dumps(request["params"]) + "\n")
        result = {"content": [{"type": "text", "text": json.dumps(request["params"]["arguments"])}],
                  "isError": False}
    else:
        result = {}
    reply = {"jsonrpc": "2.0", "id": request["id"]}
    reply.update({"error": error} if error else {"result": result})
    print(json.dumps(reply), flush=True)
