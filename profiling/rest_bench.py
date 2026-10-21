#!/usr/bin/env python3
"""Linux release comparison, requiring successful HTTP requests.
Start target/release/profiling_server first. Save a baseline binary before edits.
python3 profiling/rest_bench.py --baseline /tmp/k6-rs-before --output /tmp/rest-perf
"""
import argparse
import json
import socket
import statistics
import subprocess
import time
import urllib.error
import urllib.request
from pathlib import Path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--candidate", default="target/release/k6-rs")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--vus", type=int, default=1)
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--duration", default="3s")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    rows = []
    for repetition in range(args.repetitions):
        for label, binary in [("baseline", args.baseline), ("disabled", args.candidate), ("polling", args.candidate)]:
            stem = args.output / f"{repetition}-{label}"
            cmd = ["/usr/bin/time", "-f", "%U %S %M", "-o", str(stem) + ".time", binary,
                   "run", "profiling/bench.js", "--vus", str(args.vus), "--duration", args.duration,
                   "--summary-export", str(stem) + ".json"]
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                port = sock.getsockname()[1]
            if label == "polling":
                cmd += ["--address", f"127.0.0.1:{port}"]
            calls = 0
            with open(str(stem) + ".log", "w") as log:
                proc = subprocess.Popen(cmd, stdout=log, stderr=log)
                try:
                    while proc.poll() is None:
                        if label == "polling":
                            try:
                                with urllib.request.urlopen(f"http://127.0.0.1:{port}/v1/status", timeout=.5) as response:
                                    assert response.status == 200
                                    assert json.load(response)["data"]["type"] == "status"
                                    calls += 1
                            except (OSError, urllib.error.URLError):
                                pass  # Startup and shutdown can race the poll.
                        time.sleep(.1)
                finally:
                    if proc.poll() is None:
                        proc.terminate()
                        try:
                            proc.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            proc.kill()
                            proc.wait()
            assert proc.returncode == 0, f"See {stem}.log"
            metrics = json.loads(Path(str(stem) + ".json").read_text())["metrics"]
            def value(name, key):
                return metrics[name]["values"][key]
            user, system, rss = map(float, Path(str(stem) + ".time").read_text().split())
            row = dict(label=label, repetition=repetition, vus=args.vus,
                       rps=value("http_reqs", "rate"), p95_ms=value("http_req_duration", "p(95)"),
                       cpu_seconds=user + system, rss_mib=rss / 1024,
                       failed_rate=value("http_req_failed", "rate"), status_calls=calls)
            rows.append(row)
            (args.output / "results.json").write_text(json.dumps(rows, indent=2))
            assert value("http_reqs", "count") > 0 and row["failed_rate"] == 0, f"Unreliable workload: {row}"
            assert label != "polling" or calls > 0, "REST server was never reachable"
    for label in ["baseline", "disabled", "polling"]:
        group = [row for row in rows if row["label"] == label]
        print(label, {key: round(statistics.median(row[key] for row in group), 3)
                      for key in ["rps", "p95_ms", "cpu_seconds", "rss_mib"]})


if __name__ == "__main__":
    main()
