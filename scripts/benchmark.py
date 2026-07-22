#!/usr/bin/env python3
"""Times invoke/query latency AND samples container CPU/memory for
chaincodes already installed on a running Fabric test-network (see
scripts/benchmark.sh, which sets one up with basic-go/basic-rust/basic-ts
and calls this).

Latency here is END-TO-END through the `peer chaincode` CLI: process spawn
+ TLS handshake + gRPC to the peer + endorsement + the peer-to-chaincode RPC
+ response. It is NOT a microbenchmark of chaincode execution time alone —
CLI/network overhead likely dominates the small differences between
chaincode runtimes at this scale. Treat latency results as "these three
chaincodes perform comparably under real Fabric traffic", not as a precise
measurement of any one language's execution speed.

CPU/memory come from `docker stats` on each chaincode's own container —
that number IS specific to the chaincode process itself (not the peer or
CLI), so it's a fair per-runtime comparison of runtime footprint, sampled
both idle (before any load) and while the invoke/query loop is running.
See docs/verification.md.
"""
import argparse
import os
import statistics
import subprocess
import sys
import threading
import time


def build_env(test_network_dir):
    env = os.environ.copy()
    bin_dir = os.path.join(test_network_dir, "..", "bin")
    env["PATH"] = f"{bin_dir}:{env['PATH']}"
    env["FABRIC_CFG_PATH"] = os.path.join(test_network_dir, "..", "config")
    env["FABRIC_LOGGING_SPEC"] = "warning"
    env["CORE_PEER_TLS_ENABLED"] = "true"
    env["CORE_PEER_LOCALMSPID"] = "Org1MSP"
    env["CORE_PEER_TLS_ROOTCERT_FILE"] = os.path.join(
        test_network_dir,
        "organizations/peerOrganizations/org1.example.com/tlsca/tlsca.org1.example.com-cert.pem",
    )
    env["CORE_PEER_MSPCONFIGPATH"] = os.path.join(
        test_network_dir,
        "organizations/peerOrganizations/org1.example.com/users/Admin@org1.example.com/msp",
    )
    env["CORE_PEER_ADDRESS"] = "localhost:7051"
    return env


def run(args, env, cwd, timeout):
    start = time.perf_counter()
    result = subprocess.run(args, env=env, cwd=cwd, capture_output=True, text=True, timeout=timeout)
    return time.perf_counter() - start, result.returncode, result.stdout, result.stderr


def stats(latencies_s):
    ms = sorted(x * 1000 for x in latencies_s)
    n = len(ms)
    if n == 0:
        return {"n": 0, "min_ms": 0, "mean_ms": 0, "p50_ms": 0, "p95_ms": 0, "max_ms": 0, "throughput_ops_s": 0}
    return {
        "n": n,
        "min_ms": ms[0],
        "mean_ms": statistics.mean(ms),
        "p50_ms": ms[n // 2],
        "p95_ms": ms[min(n - 1, int(n * 0.95))],
        "max_ms": ms[-1],
        "throughput_ops_s": n / sum(latencies_s),
    }


def _parse_mem_to_mb(mem_str):
    # docker stats MemUsage looks like "12.34MiB / 512MiB" — take the used side.
    used = mem_str.split("/")[0].strip()
    for unit, factor in (("GiB", 1024.0), ("MiB", 1.0), ("KiB", 1.0 / 1024.0), ("B", 1.0 / (1024.0 * 1024.0))):
        if used.endswith(unit):
            return float(used[: -len(unit)]) * factor
    return float("nan")


def docker_stats_once(container):
    """One (cpu_percent, mem_mb) snapshot for a running container, or None."""
    try:
        out = subprocess.run(
            ["docker", "stats", "--no-stream", "--format", "{{.CPUPerc}}\t{{.MemUsage}}", container],
            capture_output=True, text=True, timeout=10,
        )
    except subprocess.TimeoutExpired:
        return None
    line = out.stdout.strip()
    if out.returncode != 0 or not line:
        return None
    cpu_str, mem_str = line.split("\t")
    return float(cpu_str.rstrip("%")), _parse_mem_to_mb(mem_str)


class ResourceSampler:
    """Polls `docker stats` for one container on a background thread."""

    def __init__(self, container, interval_s=0.3):
        self.container = container
        self.interval_s = interval_s
        self.samples = []
        self._stop = threading.Event()
        self._thread = None

    def _loop(self):
        while not self._stop.is_set():
            sample = docker_stats_once(self.container)
            if sample is not None:
                self.samples.append(sample)
            self._stop.wait(self.interval_s)

    def start(self):
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=5)

    def summary(self):
        if not self.samples:
            return {"n": 0, "cpu_avg": 0, "cpu_peak": 0, "mem_avg_mb": 0, "mem_peak_mb": 0}
        cpus = [s[0] for s in self.samples]
        mems = [s[1] for s in self.samples]
        return {
            "n": len(self.samples),
            "cpu_avg": statistics.mean(cpus),
            "cpu_peak": max(cpus),
            "mem_avg_mb": statistics.mean(mems),
            "mem_peak_mb": max(mems),
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--test-network-dir", required=True, help="path to fabric-samples/test-network")
    parser.add_argument("--channel", default="mychannel")
    parser.add_argument("--chaincodes", default="basic-go,basic-rust,basic-ts")
    parser.add_argument("-n", "--num-calls", type=int, default=30)
    parser.add_argument("--run-id", default="bench", help="prefix for created asset IDs, to avoid collisions across runs")
    args = parser.parse_args()

    test_network_dir = os.path.abspath(args.test_network_dir)
    env = build_env(test_network_dir)
    orderer_ca = os.path.join(
        test_network_dir,
        "organizations/ordererOrganizations/example.com/orderers/orderer.example.com/msp/tlscacerts/tlsca.example.com-cert.pem",
    )
    peer0_org1_ca = env["CORE_PEER_TLS_ROOTCERT_FILE"]
    peer0_org2_ca = os.path.join(
        test_network_dir, "organizations/peerOrganizations/org2.example.com/tlsca/tlsca.org2.example.com-cert.pem"
    )
    chaincodes = args.chaincodes.split(",")

    def query_args(cc, func, cc_args):
        return ["peer", "chaincode", "query", "-C", args.channel, "-n", cc,
                "-c", f'{{"function":"{func}","Args":{cc_args}}}']

    def invoke_args(cc, func, cc_args):
        return ["peer", "chaincode", "invoke", "-o", "localhost:7050",
                "--ordererTLSHostnameOverride", "orderer.example.com", "--tls", "--cafile", orderer_ca,
                "-C", args.channel, "-n", cc,
                "--peerAddresses", "localhost:7051", "--tlsRootCertFiles", peer0_org1_ca,
                "--peerAddresses", "localhost:9051", "--tlsRootCertFiles", peer0_org2_ca,
                "-c", f'{{"function":"{func}","Args":{cc_args}}}']

    results = {}
    for cc in chaincodes:
        container = f"{cc}-cc"

        print(f"--- {cc}: warming up ---", file=sys.stderr)
        run(query_args(cc, "ReadAsset", '["asset1"]'), env, test_network_dir, 15)

        idle = docker_stats_once(container)
        print(f"--- {cc}: idle = {idle} (cpu%, mem MB) ---", file=sys.stderr)

        sampler = ResourceSampler(container)
        sampler.start()

        print(f"--- {cc}: {args.num_calls} queries (ReadAsset) ---", file=sys.stderr)
        query_lat = []
        for i in range(args.num_calls):
            elapsed, code, _out, err = run(query_args(cc, "ReadAsset", '["asset1"]'), env, test_network_dir, 15)
            if code != 0:
                print(f"  query {i} FAILED: {err.strip()[-300:]}", file=sys.stderr)
                continue
            query_lat.append(elapsed)

        print(f"--- {cc}: {args.num_calls} invokes (CreateAsset, unique ids) ---", file=sys.stderr)
        invoke_lat = []
        for i in range(args.num_calls):
            asset_id = f"{args.run_id}-{cc}-{i}"
            cc_args = f'["{asset_id}","purple","1","bench","1"]'
            elapsed, code, _out, err = run(invoke_args(cc, "CreateAsset", cc_args), env, test_network_dir, 20)
            if code != 0:
                print(f"  invoke {i} FAILED: {err.strip()[-300:]}", file=sys.stderr)
                continue
            invoke_lat.append(elapsed)

        sampler.stop()
        resource = sampler.summary()
        resource["idle_mem_mb"] = idle[1] if idle else float("nan")
        resource["idle_cpu"] = idle[0] if idle else float("nan")

        results[cc] = {"query": stats(query_lat), "invoke": stats(invoke_lat), "resource": resource}

    print()
    print(f"{'Chaincode':<12} {'Op':<8} {'n':>4} {'min':>8} {'mean':>8} {'p50':>8} {'p95':>8} {'max':>8} {'ops/s':>8}")
    for cc in chaincodes:
        for op in ("query", "invoke"):
            s = results[cc][op]
            print(
                f"{cc:<12} {op:<8} {s['n']:>4} "
                f"{s['min_ms']:>7.1f} {s['mean_ms']:>7.1f} {s['p50_ms']:>7.1f} "
                f"{s['p95_ms']:>7.1f} {s['max_ms']:>7.1f} {s['throughput_ops_s']:>7.1f}"
            )

    print()
    print(f"{'Chaincode':<12} {'idle MB':>9} {'avg MB':>9} {'peak MB':>9} {'avg CPU%':>9} {'peak CPU%':>10} {'samples':>8}")
    for cc in chaincodes:
        r = results[cc]["resource"]
        print(
            f"{cc:<12} {r['idle_mem_mb']:>8.1f} {r['mem_avg_mb']:>8.1f} {r['mem_peak_mb']:>8.1f} "
            f"{r['cpu_avg']:>8.1f} {r['cpu_peak']:>9.1f} {r['n']:>8}"
        )


if __name__ == "__main__":
    main()
