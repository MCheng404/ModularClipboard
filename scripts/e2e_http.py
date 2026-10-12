import subprocess, json, sys, tempfile, time, urllib.request, urllib.error, os

exe = r"D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"
data = tempfile.mkdtemp(prefix="mcp_http_")
PORT = 9123
# 写带 token 的配置
cfg = {"mcp": {"enabled": True, "host": "127.0.0.1", "port": PORT, "token": "secret"}}
with open(os.path.join(data, "config.json"), "w", encoding="utf-8") as f:
    json.dump(cfg, f)

p = subprocess.Popen([exe, "--mcp-http", "--data-dir", data],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)

base = f"http://127.0.0.1:{PORT}"
ok = True

def post(url, token=None, body=None):
    data_b = json.dumps(body or {"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).encode()
    req = urllib.request.Request(url, data=data_b, method="POST")
    req.add_header("Content-Type", "application/json")
    if token is not None:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        r = urllib.request.urlopen(req)
        return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()

# 1. health
try:
    r = urllib.request.urlopen(base + "/health")
    print(("PASS" if r.status == 200 else "FAIL"), "GET /health ->", r.status)
    ok &= r.status == 200
except Exception as e:
    print("FAIL health:", e); ok = False

# 2. no auth -> 401
s, _ = post(base + "/mcp")
print(("PASS" if s == 401 else "FAIL"), "POST no auth ->", s); ok &= s == 401

# 3. wrong token -> 401
s, _ = post(base + "/mcp", token="wrong")
print(("PASS" if s == 401 else "FAIL"), "POST wrong token ->", s); ok &= s == 401

# 4. correct token -> 200 initialize
s, body = post(base + "/mcp", token="secret")
r = json.loads(body)
good = s == 200 and r["result"]["serverInfo"]["name"] == "modular-clipboard"
print(("PASS" if good else "FAIL"), "POST correct token ->", s, r.get("result",{}).get("serverInfo"))
ok &= good

# 5. tools/list with auth
s, body = post(base + "/mcp", token="secret",
               body={"jsonrpc":"2.0","id":2,"method":"tools/list"})
r = json.loads(body)
n = len(r["result"]["tools"])
print(("PASS" if n == 8 else "FAIL"), "tools/list ->", n, "tools"); ok &= n == 8

p.terminate()
sys.exit(0 if ok else 1)
