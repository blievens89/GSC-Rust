import json, subprocess, time, statistics, os

# Point these at your servers (BENCH_PY_CMD is optional — space-separated command).
# BENCH_SITE must be a GSC property you have access to, e.g. "https://www.example.com/".
SERVERS = {
    "python": os.environ.get("BENCH_PY_CMD", "python server.py").split(),
    "rust":   [os.environ.get("BENCH_RS_BIN", "./target/release/gsc-mcp-rs")],
}
SITE = os.environ["BENCH_SITE"]

def rpc(id, method, params=None):
    m = {"jsonrpc": "2.0", "id": id, "method": method}
    if params: m["params"] = params
    return json.dumps(m) + "\n"

INIT = rpc(1, "initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                             "clientInfo": {"name": "bench", "version": "0"}})
INITIALIZED = json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n"

def read_response(proc, want_id):
    while True:
        line = proc.stdout.readline()
        if not line: raise RuntimeError("server exited")
        try: r = json.loads(line)
        except json.JSONDecodeError: continue
        if r.get("id") == want_id: return r

def rss_kb(pid):
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], capture_output=True, text=True)
    return int(out.stdout.strip() or 0)

def spawn(cmd):
    return subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True, bufsize=1)

results = {}
for name, cmd in SERVERS.items():
    # --- cold start: spawn -> initialize response, 5 runs ---
    starts = []
    for _ in range(5):
        t0 = time.perf_counter()
        p = spawn(cmd)
        p.stdin.write(INIT); p.stdin.flush()
        read_response(p, 1)
        starts.append((time.perf_counter() - t0) * 1000)
        p.kill(); p.wait()

    # --- long-lived session: RAM + latencies ---
    p = spawn(cmd)
    p.stdin.write(INIT); p.stdin.flush(); read_response(p, 1)
    p.stdin.write(INITIALIZED); p.stdin.flush()
    time.sleep(0.3)
    ram_idle = rss_kb(p.pid)

    # tools/list latency (pure server overhead, no network), 20 reps
    tl = []
    for i in range(20):
        t0 = time.perf_counter()
        p.stdin.write(rpc(100+i, "tools/list")); p.stdin.flush()
        read_response(p, 100+i)
        tl.append((time.perf_counter() - t0) * 1000)

    # real API call: list_sites (network-bound)
    t0 = time.perf_counter()
    p.stdin.write(rpc(200, "tools/call", {"name": "list_sites", "arguments": {}})); p.stdin.flush()
    read_response(p, 200)
    api_cold = (time.perf_counter() - t0) * 1000

    # search_analytics: first call = network, second identical = response-cache hit
    args = {"site_url": SITE, "row_limit": 1000}
    t0 = time.perf_counter()
    p.stdin.write(rpc(201, "tools/call", {"name": "top_queries", "arguments": {"site_url": SITE, "limit": 100}})); p.stdin.flush()
    r1 = read_response(p, 201)
    sa_net = (time.perf_counter() - t0) * 1000
    cache_runs = []
    for i in range(5):
        t0 = time.perf_counter()
        p.stdin.write(rpc(210+i, "tools/call", {"name": "top_queries", "arguments": {"site_url": SITE, "limit": 100}})); p.stdin.flush()
        read_response(p, 210+i)
        cache_runs.append((time.perf_counter() - t0) * 1000)

    ram_after = rss_kb(p.pid)
    rows = json.loads(r1["result"]["content"][0]["text"]).get("row_count", "?")
    p.kill(); p.wait()

    results[name] = dict(
        start_ms=statistics.median(starts), start_all=[round(x) for x in starts],
        ram_idle_mb=ram_idle/1024, ram_after_mb=ram_after/1024,
        tools_list_ms=statistics.median(tl),
        api_cold_ms=api_cold, sa_net_ms=sa_net,
        cache_ms=statistics.median(cache_runs), rows=rows,
    )

py, rs = results["python"], results["rust"]
print(f"{'metric':<38}{'python':>12}{'rust':>12}{'ratio':>10}")
def line(label, a, b, unit="ms", better="lower"):
    ratio = a/b if b else 0
    print(f"{label:<38}{a:>10.1f} {unit:<2}{b:>9.1f} {unit:<2}{ratio:>8.1f}x")
line("cold start (spawn->initialize)", py["start_ms"], rs["start_ms"])
line("RAM after init (RSS)", py["ram_idle_mb"], rs["ram_idle_mb"], "MB")
line("RAM after API calls (RSS)", py["ram_after_mb"], rs["ram_after_mb"], "MB")
line("tools/list (server overhead)", py["tools_list_ms"], rs["tools_list_ms"])
line("list_sites (network-bound)", py["api_cold_ms"], rs["api_cold_ms"])
line(f"top_queries {py['rows']} rows (network)", py["sa_net_ms"], rs["sa_net_ms"])
line("top_queries repeat (cache hit)", py["cache_ms"], rs["cache_ms"])
print("\ncold start runs (ms): python", py["start_all"], "| rust", rs["start_all"])
