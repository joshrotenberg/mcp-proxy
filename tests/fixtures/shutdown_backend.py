"""PID-observable MCP fixture that deliberately ignores stdin EOF and SIGTERM."""
import json
import os
from pathlib import Path
import signal
import sys
import time

pid_file = Path(sys.argv[1])
mode = sys.argv[2]
pid_file.write_text(str(os.getpid()))
signal.signal(signal.SIGTERM, signal.SIG_IGN)
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request["method"]
    if method == "initialize":
        Path(str(pid_file) + ".initialize").touch()
        if mode == "hang-init":
            while True:
                time.sleep(0.01)
        result = {"protocolVersion": request["params"]["protocolVersion"],
                  "capabilities": {"tools": {}},
                  "serverInfo": {"name": "shutdown-fixture", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "work", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        if request["params"].get("arguments", {}).get("wait", False):
            Path(str(pid_file) + ".called").touch()
            while not Path(str(pid_file) + ".release").exists():
                time.sleep(0.01)
        result = {"content": [{"type": "text", "text": "completed"}], "isError": False}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
# No EOF exit: the owner must explicitly terminate and reap this process.
while True:
    time.sleep(1)
