#!/usr/bin/env python3
"""Real HTTP A/B/C sync and offline restore acceptance; uses only stdlib."""
import hashlib
import json
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request

SECRET = "sync-smoke-token-at-least-24-bytes"


def sha(data):
    return hashlib.sha256(data).hexdigest()


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False).encode()


class Server:
    def __init__(self, binary, directory, root):
        self.binary = binary
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            self.port = sock.getsockname()[1]
        self.config = directory / (root.name + ".toml")
        self.config.write_text(f"listen='127.0.0.1:{self.port}'\nexecution=false\nlog_level='off'\ntoken='{SECRET}'\n[sync]\nenabled=true\nroot='{root}'\nmetadata_reserve_bytes=0\n")
        self.process = None
        self.epoch = None
        self.identity = None

    def admin(self, command, *args):
        completed = subprocess.run([self.binary, "sync", command, str(self.config), *map(str, args)], check=True, capture_output=True)
        return json.loads(completed.stdout)

    def start(self):
        self.process = subprocess.Popen([self.binary, str(self.config)], stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        for _ in range(100):
            if self.process.poll() is not None:
                raise RuntimeError(self.process.stderr.read().decode())
            try:
                self.identity = self.call("GET", "capabilities")
                self.epoch = self.identity["historyEpoch"]
                return
            except (OSError, urllib.error.URLError):
                time.sleep(0.05)
        raise RuntimeError("server startup timed out")

    def stop(self):
        if self.process and self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
            self.process.wait(timeout=15)
        if self.process:
            self.process.stderr.close()

    def call(self, method, path, body=None, expected=200):
        raw = body if isinstance(body, bytes) else canonical(body) if body is not None else None
        request = urllib.request.Request(f"http://127.0.0.1:{self.port}/v1/sync/{path}", data=raw, method=method)
        request.add_header("Authorization", f"Bearer {SECRET}")
        if self.epoch:
            request.add_header("X-Sync-History-Epoch", self.epoch)
        if raw is not None:
            request.add_header("Content-Type", "application/octet-stream" if isinstance(body, bytes) else "application/json")
        try:
            response = urllib.request.urlopen(request, timeout=10)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            data = response.read()
            assert response.status == expected, (response.status, data)
            return data if "/objects/" in path else json.loads(data)

    def command(self, replica, seq, path, extra, expected=200):
        body = {"operationId": f"{replica}-{seq}", "replicaId": replica, "opSeq": str(seq), "authorityId": self.identity["authorityId"], "historyEpoch": self.epoch, "expectedProjectLifecycleRevision": "1", **extra}
        return self.call("POST", path, body, expected)

    def replica(self, name):
        self.call("POST", "replicas", {"replicaId": name})
        projects = self.call("GET", "projects?state=all")["projects"]
        scopes = [{"projectId": p["projectId"], "cursor": self.call("GET", f"projects/{p['projectId']}/datasets?state=all")["cursor"]} for p in projects]
        self.call("POST", f"replicas/{name}/activate", {"scopes": scopes})

    def upload(self, text):
        data = text.encode()
        content_hash = sha(data)
        self.call("PUT", f"projects/p/objects/{content_hash}", data)
        manifest = canonical({"format": "fs-agent.files", "version": 1, "entries": [{"path": "note.txt", "kind": "file", "hash": content_hash, "size": str(len(data))}]})
        manifest_hash = sha(manifest)
        self.call("PUT", f"projects/p/objects/{manifest_hash}", manifest)
        return manifest_hash

    def head(self):
        value = self.call("GET", "projects/p/datasets/files/head")
        return {"generation": value["generation"], "manifestHash": value["manifestHash"]}


def verify_history(server):
    versions = server.call("GET", "projects/p/datasets/files/versions")["versions"]
    count = 0
    for version in versions:
        assert version["contentStatus"] == "available"
        manifest_hash = version["manifestHash"]
        raw = server.call("GET", f"projects/p/objects/{manifest_hash}")
        assert sha(raw) == manifest_hash
        for entry in json.loads(raw)["entries"]:
            if entry["kind"] == "file":
                content = server.call("GET", f"projects/p/objects/{entry['hash']}")
                assert sha(content) == entry["hash"]
                assert len(content) == int(entry["size"])
                count += 1
    return count


def devices(server):
    for replica in ("A", "B", "C"):
        server.replica(replica)
    server.command("A", 1, "projects", {"projectId": "p"})
    first = server.upload("first")
    server.command("A", 2, "projects/p/datasets", {"datasetId": "files", "logicalId": "files", "kind": "files", "manifestHash": first})
    old = server.head()
    next_manifest = server.upload("second")
    plan = {"expectedHead": old, "nextManifestHash": next_manifest}
    receipt = server.command("A", 3, "projects/p/datasets/files/publish", plan)
    assert server.command("A", 3, "projects/p/datasets/files/publish", plan) == receipt
    conflict = server.command("B", 1, "projects/p/datasets/files/publish", plan, 412)
    assert conflict["code"] == "HEAD_CONFLICT"
    server.command("B", 2, "projects/p/datasets/files/publish", {"expectedHead": server.head(), "nextManifestHash": next_manifest})
    third = server.upload("third")
    server.command("C", 1, "projects/p/datasets/files/publish", {"expectedHead": server.head(), "nextManifestHash": third})
    assert server.head()["generation"] == "3"
    return server.head()


def main():
    default_binary = Path(__file__).resolve().parents[1] / "target/debug/cells"
    binary = str(Path(sys.argv[1] if len(sys.argv) > 1 else default_binary).resolve())
    with tempfile.TemporaryDirectory(prefix="cells-sync-http-") as temporary:
        directory = Path(temporary)
        original = Server(binary, directory, directory / "root")
        restored = Server(binary, directory, directory / "restored")
        try:
            original.admin("init")
            original.start()
            head = devices(original)
            assert verify_history(original) == 3
            epoch = original.epoch
            original.stop()
            original.admin("backup", directory / "backup")
            (directory / "root").rename(directory / "original-unavailable")
            restored.admin("restore", directory / "backup")
            restored.start()
            assert restored.epoch != epoch and restored.head() == head
            assert verify_history(restored) == 3
            restored.replica("new-device")
            restored.command("new-device", 1, "projects", {"projectId": "after-restore"})
            verify_epoch_fence(restored, epoch)
            print(json.dumps({"devices": 3, "restoredVersions": 3, "httpHashesVerified": True, "epochChanged": True, "oldEpochRejected": True}))
        finally:
            original.stop()
            restored.stop()


def verify_epoch_fence(server, old_epoch):
    server.replica("A")
    receipt = server.command("A", 1, "projects", {"projectId": "epoch-check"})
    new_epoch = server.epoch
    server.epoch = old_epoch
    try:
        for method, path, body in [
            ("GET", "replicas/A/operations/1", None),
            ("POST", "replicas/A/operations/1/cancel", {}),
        ]:
            assert server.call(method, path, body, 409)["code"] == "HISTORY_EPOCH_CHANGED"
        assert server.command("A", 1, "projects", {"projectId": "epoch-check"}, 409)["code"] == "HISTORY_EPOCH_CHANGED"
    finally:
        server.epoch = new_epoch
    assert server.call("GET", "replicas/A/operations/1") == receipt


if __name__ == "__main__":
    main()
