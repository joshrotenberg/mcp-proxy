"""Structured Helm regressions; optionally run the rendered config with the real CLI.

Requires Helm 3, Python 3.11+ and PyYAML (CI pins 6.0.3). No Kubernetes cluster
is needed: the runtime check verifies the application listener, not cluster DNS,
kube-proxy, a rolling upgrade, or the Kubernetes Secret controller.
"""

import argparse
import json
import pathlib
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import urllib.error
import urllib.request

import yaml

REPO = pathlib.Path(__file__).resolve().parents[1]
CHART = REPO / "charts/mcp-proxy"
VERSION = tomllib.loads((REPO / "Cargo.toml").read_text())["package"]["version"]


def helm(values, directory, *, invalid=False):
    path = directory / "values.yaml"
    path.write_text(yaml.safe_dump(values))
    result = subprocess.run(
        ["helm", "template", "health-check", str(CHART), "-f", str(path)],
        capture_output=True, text=True, timeout=20, check=False,
    )
    if invalid:
        assert result.returncode != 0, f"accepted invalid values: {values}"
        assert "schema" in result.stderr, result.stderr
        return None
    assert result.returncode == 0, result.stderr
    return {doc["kind"]: doc for doc in yaml.safe_load_all(result.stdout) if doc}


def pod_template(docs):
    return docs["Deployment"]["spec"]["template"]


def annotations(docs):
    return pod_template(docs)["metadata"]["annotations"]


def config(docs):
    return docs["ConfigMap"]["data"]["proxy.toml"]


def ports(docs, service, application):
    container = pod_template(docs)["spec"]["containers"][0]
    assert container["image"] == f"ghcr.io/joshrotenberg/mcp-proxy:{VERSION}"
    assert container["ports"] == [
        {"name": "http", "containerPort": application, "protocol": "TCP"}
    ]
    for kind, path in [("livenessProbe", "/livez"), ("readinessProbe", "/readyz")]:
        assert container[kind]["httpGet"] == {"path": path, "port": "http"}
    assert docs["Service"]["spec"]["ports"] == [
        {"port": service, "targetPort": "http", "protocol": "TCP", "name": "http"}
    ]
    assert docs["Pod"]["spec"]["containers"][0]["args"][-1] == (
        f"http://health-check-mcp-proxy:{service}/readyz"
    )
    assert tomllib.loads(config(docs))["proxy"]["listen"] == {
        "host": "0.0.0.0", "port": application
    }


def render_checks(directory):
    subprocess.run(["helm", "lint", str(CHART), "--strict"], check=True, timeout=20)
    base = helm({}, directory)
    assert base["Service"]["spec"]["sessionAffinity"] == "ClientIP"
    assert base["Service"]["spec"]["sessionAffinityConfig"] == {"clientIP": {"timeoutSeconds": 10800}}
    for values in [{"replicaCount": 2}, {"autoscaling": {"enabled": True, "minReplicas": 2}}]:
        multiple = helm(values, directory)
        assert multiple["Service"]["spec"]["sessionAffinity"] == "ClientIP"
        assert ("replicas" in multiple["Deployment"]["spec"]) != values.get("autoscaling", {}).get("enabled", False)
    external = helm({"service": {"sessionAffinity": "None"}}, directory)
    assert external["Service"]["spec"]["sessionAffinity"] == "None"
    assert "sessionAffinityConfig" not in external["Service"]["spec"]
    for timeout in [1, 86400]:
        bounded = helm({"service": {"sessionAffinityTimeoutSeconds": timeout}}, directory)
        assert bounded["Service"]["spec"]["sessionAffinityConfig"]["clientIP"]["timeoutSeconds"] == timeout
    for invalid in [0, -1, 86401, "10800", 1.5, None]:
        helm({"service": {"sessionAffinityTimeoutSeconds": invalid}}, directory, invalid=True)
    for invalid in ["none", "Cookie", None]:
        helm({"service": {"sessionAffinity": invalid}}, directory, invalid=True)
    for values, service, application in [
        ({}, 8080, 8080),
        ({"service": {"port": 9090}}, 9090, 8080),
        ({"containerPort": 8181}, 8080, 8181),
        ({"service": {"port": 9090}, "containerPort": 8181}, 9090, 8181),
        ({"service": {"port": 1}, "containerPort": 65535}, 1, 65535),
    ]:
        ports(helm(values, directory), service, application)
    # A Service-only change must leave the entire pod template unchanged.
    assert pod_template(base) == pod_template(helm({"service": {"port": 9090}}, directory))

    raw = yaml.safe_load((CHART / "values.yaml").read_text())["config"]
    auth = {
        "containerPort": 8181,
        "config": raw + '\n[security]\nadmin_token = "${ADMIN_TOKEN}"\n',
        "env": [{"name": "ADMIN_TOKEN", "valueFrom": {
            "secretKeyRef": {"name": "admin-credentials", "key": "token"}
        }}],
        "podAnnotations": {"example.com/restart": "revision-1"},
    }
    authenticated = helm(auth, directory)
    subprocess.run(["helm", "lint", str(CHART), "--strict", "-f", str(directory / "values.yaml")],
                   check=True, timeout=20)
    ports(authenticated, 8080, 8181)
    assert pod_template(authenticated)["spec"]["containers"][0]["env"] == auth["env"]
    assert annotations(authenticated)["example.com/restart"] == "revision-1"
    assert tomllib.loads(config(authenticated))["security"]["admin_token"] == "${ADMIN_TOKEN}"
    external_restart = helm({**auth, "podAnnotations": {"example.com/restart": "revision-2"}}, directory)
    assert pod_template(authenticated) != pod_template(external_restart)
    for key in ["checksum/config", "checksum/secret"]:
        assert annotations(authenticated)[key] == annotations(external_restart)[key]
    # Existing literal TOML remains valid; custom listeners must match containerPort.
    ports(helm({"containerPort": 8181, "config": raw.replace(
        "{{ .Values.containerPort }}", "8181"
    )}, directory), 8080, 8181)
    literal = helm({"config": raw + '\n# Literal delimiter: {{ "{{" }}\n'}, directory)
    assert "# Literal delimiter: {{" in config(literal)

    changed_config = helm({"config": raw + "\n# config-only revision\n"}, directory)
    assert annotations(base)["checksum/config"] != annotations(changed_config)["checksum/config"]
    assert annotations(base)["checksum/secret"] == annotations(changed_config)["checksum/secret"]
    first = helm({"secrets": {"token": "first", "other": "unchanged"}}, directory)
    rotated = helm({"secrets": {"token": "rotated", "other": "unchanged"}}, directory)
    reordered = helm({"secrets": {"other": "unchanged", "token": "first"}}, directory)
    assert first["Secret"]["stringData"]["token"] == "first"
    assert rotated["Secret"]["stringData"]["token"] == "rotated"
    assert "Secret" not in base
    assert annotations(first)["checksum/secret"] != annotations(rotated)["checksum/secret"]
    assert annotations(first)["checksum/secret"] != annotations(base)["checksum/secret"]
    assert annotations(first)["checksum/config"] == annotations(rotated)["checksum/config"]
    assert pod_template(first) == pod_template(reordered)
    for docs in [base, first, rotated, changed_config]:
        for key in ["checksum/config", "checksum/secret"]:
            value = annotations(docs)[key]
            assert len(value) == 64 and all(c in "0123456789abcdef" for c in value)

    for key in ["containerPort", "service"]:
        for value in [0, -1, 65536, "9090", 1.5, None]:
            values = {key: {"port": value} if key == "service" else value}
            helm(values, directory, invalid=True)
    for reserved in ["checksum/config", "checksum/secret"]:
        helm({"podAnnotations": {reserved: "override"}}, directory, invalid=True)
    print("Helm ports, authenticated probes, schema and config/Secret rollout regressions passed")


def runtime_check(binary, directory):
    # Select an available nondefault application port; a bind race is a failure,
    # never an excuse to retry MCP initialization or silently skip the smoke test.
    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    assert port != 8080
    docs = helm({"containerPort": port, "service": {"port": 9090}}, directory)
    ports(docs, 9090, port)
    journal = directory / "calls.jsonl"
    source = config(docs) + (
        '\n[[backends]]\nname = "fixture"\ntransport = "stdio"\n'
        f"command = {json.dumps(sys.executable)}\n"
        f"args = {json.dumps([str(REPO / 'tests/fixtures/session_backend.py'), str(journal)])}\n"
    )
    config_path = directory / "proxy.toml"
    config_path.write_text(source)
    # Local test peers must not use a corporate/managed HTTP proxy.
    client = urllib.request.build_opener(urllib.request.ProxyHandler({}))
    url = f"http://127.0.0.1:{port}"
    with (directory / "proxy.log").open("w+") as log:
        process = subprocess.Popen([str(binary), "--config", str(config_path)], stdout=log, stderr=log)
        try:
            deadline = time.monotonic() + 15
            while True:
                assert process.poll() is None, "rendered-config CLI exited during startup"
                try:
                    with client.open(url + "/readyz", timeout=1) as response:
                        assert response.status == 200 and response.read() == b"ok"
                    break
                except urllib.error.URLError:
                    assert time.monotonic() < deadline, "nondefault listener never became ready"
                    time.sleep(0.05)
            with client.open(url + "/livez", timeout=2) as response:
                assert response.status == 200 and response.read() == b"ok"
            headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                       "Mcp-Protocol-Version": "2025-11-25"}

            def request(body, status=200):
                req = urllib.request.Request(url + "/", json.dumps(body).encode(), headers)
                with client.open(req, timeout=5) as response:
                    assert response.status == status
                    text = response.read().decode()
                    if status == 202:
                        assert not text
                        return None, response.headers
                    frames = [line[6:] for line in text.splitlines() if line.startswith("data: ")]
                    assert len(frames) <= 1, text
                    payload = json.loads(frames[0] if frames else text)
                    assert payload["jsonrpc"] == "2.0" and payload["id"] == body["id"]
                    assert "error" not in payload, payload
                    return payload["result"], response.headers

            result, received = request({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {
                "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": {"name": "helm-smoke", "version": "1"}
            }})
            assert result["protocolVersion"] == "2025-11-25"
            headers["Mcp-Session-Id"] = received["mcp-session-id"]
            assert headers["Mcp-Session-Id"]
            request({"jsonrpc": "2.0", "method": "notifications/initialized", "params": {}}, 202)
            result, _ = request({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}})
            assert sorted(tool["name"] for tool in result["tools"]) == ["fixture/blocked", "fixture/echo"]
            result, _ = request({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
                "name": "fixture/echo", "arguments": {"port": port}
            }})
            assert result.get("isError") is not True
            assert json.loads(result["content"][0]["text"]) == {"port": port}
            assert [json.loads(line) for line in journal.read_text().splitlines()] == [
                {"name": "echo", "arguments": {"port": port}}
            ]
        except BaseException:
            log.flush()
            log.seek(0)
            print(log.read(), file=sys.stderr)
            raise
        finally:
            if process.poll() is None:
                process.terminate()
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
                raise AssertionError("rendered-config CLI did not shut down")
        assert process.returncode == 0, f"CLI exit code: {process.returncode}"
    print(f"Rendered nondefault application port {port}: probes and full MCP session passed (Service port 9090)")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, help="also run rendered config with this mcp-proxy executable")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="mcp-chart-") as temporary:
        directory = pathlib.Path(temporary)
        render_checks(directory)
        if args.binary:
            runtime_check(args.binary.resolve(), directory)
