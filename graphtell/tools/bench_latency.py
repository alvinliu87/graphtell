#!/usr/bin/env python3
"""Compare recall latency of the old vs new serve binary on CRMEB (project=1).

Scenarios:
  warmup : first query (cold: candidate loading / node encoding / persistence / query encoding all run)
  repeat : ask the same sentence again (retries / multi-turn are common in an IDE)
  diff   : switch to a different query (same project, different semantic vector)
"""
import json
import time
import urllib.request

PORTS = {"old(5177)": 5177, "new(5185)": 5185}
PROJECT = 1
QUERIES = [
    "如何修改下单优惠",
    "退款审核通过后钱怎么退回",
    "用户登录时密码怎么加密比对",
    "订单超时未支付怎么自动取消",
    "商品库存超卖怎么加锁扣减",
]

URL = "http://127.0.0.1:{port}/api/projects/{pid}/recall"


def recall(port, q, snippets=False):
    body = json.dumps({"query": q, "limit": 10, "hops": 2,
                       "with_snippets": snippets}).encode()
    req = urllib.request.Request(URL.format(port=port, pid=PROJECT),
                                 data=body,
                                 headers={"Content-Type": "application/json"})
    t0 = time.perf_counter()
    with urllib.request.urlopen(req, timeout=120) as r:
        r.read()  # Discard the response body, measure only server-side computation + transfer
    return time.perf_counter() - t0


def bench(port, label):
    print(f"\n===== {label} (port {port}) =====")
    # warmup
    t = recall(port, QUERIES[0])
    print(f"  warmup  single query {QUERIES[0][:10]}…  {t*1000:7.1f} ms")
    # Multi-turn: repeat the same sentence + diff a different one
    reps, diffs = [], []
    for i in range(5):
        q_same = QUERIES[0]
        q_diff = QUERIES[(i % (len(QUERIES) - 1)) + 1]
        tr = recall(port, q_same)
        td = recall(port, q_diff)
        reps.append(tr)
        diffs.append(td)
        print(f"  round{i+1}: repeat {tr*1000:7.1f} ms | diff {td*1000:7.1f} ms  ({q_diff[:12]}…)")
    print(f"  -> repeat avg {sum(reps)/len(reps)*1000:7.1f} ms | diff avg {sum(diffs)/len(diffs)*1000:7.1f} ms")


if __name__ == "__main__":
    for label, port in PORTS.items():
        try:
            bench(port, label)
        except Exception as e:
            print(f"  !! {label} failed: {e}")
