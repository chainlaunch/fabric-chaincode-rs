#!/usr/bin/env python3
"""Times invoke/query latency for chaincodes already installed on a running
Fabric test-network (see scripts/benchmark.sh, which sets one up with
basic-go/basic-rust/basic-ts and calls this).

This measures END-TO-END latency through the `peer chaincode` CLI: process
spawn + TLS handshake + gRPC to the peer + endorsement + the peer-to-chaincode
RPC + response. It is NOT a microbenchmark of chaincode execution time alone
— CLI/network overhead likely dominates the small differences between
chaincode runtimes at this scale. Treat results as "these three chaincodes
perform comparably under real Fabric traffic", not as a precise measurement
of any one language's execution speed. See docs/verification.md.
"""
import argparse
import os
import statistics
import subprocess
import sys
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
        print(f"--- {cc}: warming up ---", file=sys.stderr)
        run(query_args(cc, "ReadAsset", '["asset1"]'), env, test_network_dir, 15)

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

        results[cc] = {"query": stats(query_lat), "invoke": stats(invoke_lat)}

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


if __name__ == "__main__":
    main()
