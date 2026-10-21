#!/usr/bin/env python3
"""Black-box REST assertions; run against k6 v2.0.0 and k6-rs separately.
python3 crates/k6-conformance/tests/rest_api.py --bin target/debug/k6-rs
No Python dependencies. Each process owns a temporary script and loopback port.
"""
import argparse
import concurrent.futures
import hashlib
import json
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path


def check(condition, message):
    if not condition:
        raise AssertionError(message)


class Run:
    def __init__(self, binary, source, flags=()):
        self.tmp = tempfile.TemporaryDirectory(prefix="k6-rest-")
        path = Path(self.tmp.name)
        (path / "script.js").write_text(source)
        with socket.socket() as sock:
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]
        self.url = f"http://127.0.0.1:{port}"
        self.log = open(path / "output.log", "w+")
        self.proc = subprocess.Popen([binary, "run", "--address", f"127.0.0.1:{port}", *flags, str(path / "script.js")], stdout=self.log, stderr=self.log)
        for _ in range(200):
            try:
                status = self.req("GET", "/v1/status")["data"]["attributes"]
                ready = status["status"] == 4 if "--paused" in flags else status["running"]
                if ready:
                    return
                time.sleep(.025)
            except OSError:
                if self.proc.poll() is not None:
                    break
                time.sleep(.025)
        self.log.seek(0)
        message = self.log.read()
        self.close()
        raise AssertionError("server did not start: " + message)

    def req(self, method, path, body=None, expected=200, raw=False):
        if body is not None and not isinstance(body, str):
            body = json.dumps(body)
        req = urllib.request.Request(self.url + path, data=None if body is None else body.encode(), method=method)
        try:
            response = urllib.request.urlopen(req, timeout=5)
        except urllib.error.HTTPError as error:
            response = error
        data = response.read().decode()
        check(response.status == expected, f"{method} {path}: {response.status} != {expected}: {data}")
        return data if raw or not data else json.loads(data)

    def patch(self, attributes, expected=200):
        return self.req("PATCH", "/v1/status", {"data": {"type": "status", "id": "default", "attributes": attributes}}, expected)

    def close(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()
        self.log.close()
        self.tmp.cleanup()


def exercise(binary):
    source = """
import { sleep, group, check } from 'k6';
import { Trend, Counter } from 'k6/metrics';
const custom = new Trend('custom');
const timed = new Trend('timed', true);
export const options = { vus: 2, duration: '30s', thresholds: { custom: ['avg<1'] } };
export function setup() { return { token: 42 }; }
export default function(data) {
    group('api', () => check(data, { 'token': x => x.token === 42 }));
    custom.add(3); timed.add(7); sleep(.05);
}
export function teardown(data) { if (data.token !== 42) throw Error('wrong teardown data'); }
"""
    run = Run(binary, source, ["--paused", "--no-setup"])
    try:
        status = run.req("GET", "/v1/status")["data"]
        check(status["type"] == "status" and status["id"] == "default", status)
        check(status["attributes"] == {"status": 4, "paused": True, "vus": 0, "vus-max": 2, "stopped": False, "running": False, "tainted": False}, status)
        check(run.req("GET", "/v1/setup")["data"]["attributes"] == {}, "absent setup data")
        check(run.req("PUT", "/v1/setup", "null")["data"]["attributes"] == {"data": None}, "explicit null")
        check(run.req("PUT", "/v1/setup", "")["data"]["attributes"] == {}, "clear data")
        run.req("PUT", "/v1/setup", "{", 400)
        run.req("PATCH", "/v1/status", "{", 400)
        for body in ["null", "{}", '{"data":null}']:
            run.req("PATCH", "/v1/status", body)
        run.patch({"paused": True}, 500)
        for value in [0, -1, 100, "2"]:
            err = run.patch({"vus": value}, 500)
            check(err["errors"][0]["detail"] == "live VU configuration updates are not supported", err)
        run.patch({"paused": "true"}, 400)
        check(run.req("POST", "/v1/setup", "")["data"]["attributes"] == {"data": {"token": 42}}, "setup result")
        run.req("POST", "/v1/teardown", "")
        run.req("POST", "/v1/status", "", 405)
        run.req("GET", "/v1/metrics/missing", expected=404)
        run.req("GET", "/v1/groups/missing", expected=404)
        # Ordering is intentional: resume succeeds before scaling fails.
        run.patch({"paused": False, "vus-max": 100}, 500)
        time.sleep(2.2)
        status = run.req("GET", "/v1/status")["data"]["attributes"]
        check(status["running"] and not status["paused"] and status["vus"] == 2 and status["tainted"], status)
        for paused in [True, False]:
            err = run.patch({"paused": paused}, 500)
            check(err["errors"][0]["detail"] == "constant-vus executor 'default' doesn't support pause and resume operations after its start", err)
        metrics = {m["id"]: m for m in run.req("GET", "/v1/metrics")["data"]}
        for name, contains, value in [("custom", "default", 3), ("timed", "time", 7)]:
            m = run.req("GET", "/v1/metrics/" + name)["data"]
            check(m["attributes"]["contains"] == contains, m)
            check(m["attributes"]["tainted"] == (True if name == "custom" else None), m)
            check(m["attributes"]["sample"]["avg"] == value, m)
            check(set(m["attributes"]["sample"]) == {"avg", "min", "max", "med", "p(90)", "p(95)"}, m)
        check(metrics["checks"]["attributes"]["sample"] == {"rate": 1}, metrics["checks"])
        groups = run.req("GET", "/v1/groups")["data"]
        gid = hashlib.md5(b"::api").hexdigest()
        group = run.req("GET", "/v1/groups/" + gid)["data"]
        check(len(groups) == 2 and group["attributes"]["checks"][0]["fails"] == 0, groups)
        with concurrent.futures.ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(lambda _: run.patch({"vus": 0}, 500), range(24)))
        check(run.patch({"stopped": True, "paused": True, "vus": 100})["data"]["attributes"]["stopped"], "stop")
        check(run.proc.wait(timeout=5) == 103, "REST stop exit")
    finally:
        run.close()
    for operation in ["sleep(30)", "while (true) {}"]:
        run = Run(binary, "import { sleep } from 'k6';\nexport const options={vus:1,duration:'60s'};\nexport default function(){" + operation + "}")
        try:
            time.sleep(.2)
            start = time.monotonic()
            run.patch({"stopped": True})
            check(run.proc.wait(timeout=3) == 103, "interrupt exit")
            check(time.monotonic() - start < 2, "in-flight iteration was not interrupted")
        finally:
            run.close()
    # A stalled HTTP request must be interrupted, without running the rest of its iteration.
    listener = socket.socket()
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    listener.settimeout(5)
    port = listener.getsockname()[1]
    arrived, release = threading.Event(), threading.Event()
    def stalled_target():
        try:
            connection, _ = listener.accept()
            with connection:
                connection.settimeout(5)
                connection.recv(8192)
                arrived.set()
                release.wait(5)
        except OSError:
            pass
    worker = threading.Thread(target=stalled_target, daemon=True)
    worker.start()
    run = None
    try:
        run = Run(binary, "import http from 'k6/http';\nexport const options={vus:1,duration:'60s'};\nexport default function(){http.get('http://127.0.0.1:" + str(port) + "/'); console.log('ITERATION-COMPLETED');}")
        check(arrived.wait(3), "HTTP request did not reach the target")
        run.patch({"stopped": True})
        check(run.proc.wait(timeout=3) == 103, "stop during HTTP request")
        run.log.seek(0)
        check("ITERATION-COMPLETED" not in run.log.read(), "in-flight HTTP iteration completed")
    finally:
        release.set()
        listener.close()
        worker.join(timeout=6)
        if run:
            run.close()
    run = Run(binary, "export const options={vus:1,duration:'1s'};\nexport function setup(){throw Error('setup must not run');}\nexport default function(){}", ["--paused"])
    try:
        run.patch({"stopped": True})
        check(run.proc.wait(timeout=3) == 103, "stop before setup")
    finally:
        run.close()
    # A scheduled ramp parks and reactivates VUs; status reflects actual activity.
    run = Run(binary, "import { sleep } from 'k6';\nexport const options={scenarios:{load:{executor:'ramping-vus',startVUs:2,stages:[{duration:'500ms',target:2},{duration:'100ms',target:0},{duration:'700ms',target:0},{duration:'100ms',target:2},{duration:'700ms',target:2}]}}};\nexport default function(){sleep(.01);}", ["--paused"])
    try:
        run.patch({"paused": False})
        time.sleep(.25)
        check(run.req("GET", "/v1/status")["data"]["attributes"]["vus"] == 2, "ramp starts with 2")
        time.sleep(.65)
        check(run.req("GET", "/v1/status")["data"]["attributes"]["vus"] == 0, "ramp parks at zero")
        time.sleep(.7)
        check(run.req("GET", "/v1/status")["data"]["attributes"]["vus"] == 2, "ramp reactivates")
        check(run.proc.wait(timeout=4) == 0, "normal completion")
    finally:
        run.close()
    run = Run(binary, "import { sleep } from 'k6';\nexport const options={vus:1,duration:'30s'};\nexport default function(){sleep(10); }\nexport function teardown(){sleep(.5);}")
    try:
        time.sleep(.1)
        run.patch({"stopped": True})
        time.sleep(.1)
        check(run.req("GET", "/v1/status")["data"]["attributes"]["stopped"], "status during teardown")
        run.patch({"vus": 0}, 500)
        run.patch({"stopped": True})
        check(run.proc.wait(timeout=3) == 103, "repeated stop during teardown")
    finally:
        run.close()
    for result in ["null", "Promise.resolve(null)", "Promise.resolve({token:42})"]:
        run = Run(binary, "export const options={vus:1,duration:'30s'};\nexport function setup(){return " + result + ";}\nexport function teardown(){return Promise.reject(Error('teardown failed'));}\nexport default function(){}", ["--paused", "--no-setup", "--no-teardown"])
        try:
            response = run.req("POST", "/v1/setup", "")["data"]["attributes"]
            check(response == {"data": {"token": 42} if "token" in result else None}, response)
            run.req("POST", "/v1/teardown", "", 500)
            run.patch({"stopped": True})
            check(run.proc.wait(timeout=3) == 103, "stop after promise hook")
        finally:
            run.close()
    # Lifecycle timeouts are execution errors; the REST server stays usable.
    run = Run(binary, "import { sleep } from 'k6';\nexport const options={vus:1,duration:'30s',setupTimeout:'100ms',teardownTimeout:'100ms'};\nexport function setup(){sleep(30);return 1;}\nexport function teardown(){sleep(30);}\nexport default function(){}", ["--paused", "--no-setup", "--no-teardown"])
    try:
        for path in ["/v1/setup", "/v1/teardown"]:
            start = time.monotonic()
            run.req("POST", path, "", 500)
            check(time.monotonic() - start < 2, "lifecycle timeout")
        run.patch({"stopped": True})
        check(run.proc.wait(timeout=3) == 103, "stop after lifecycle error")
    finally:
        run.close()
    # REST abort after execution started still runs teardown, even during setup.
    run = Run(binary, "import { sleep } from 'k6';\nexport const options={vus:1,duration:'30s'};\nexport function setup(){sleep(30);return 1;}\nexport function teardown(){console.log('TEARDOWN-RAN');}\nexport default function(){}")
    try:
        time.sleep(.1)
        run.patch({"stopped": True})
        check(run.proc.wait(timeout=3) == 103, "stop during setup")
        run.log.seek(0)
        check("TEARDOWN-RAN" in run.log.read(), "teardown after aborted setup")
    finally:
        run.close()
    print(f"PASS: {binary}: REST envelopes, setup data/hooks, metrics/groups, resume ordering, concurrent commands, immediate stop (sleep and CPU), stop before setup")


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", required=True)
    exercise(parser.parse_args().bin)
