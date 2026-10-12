import subprocess, json, tempfile, time, urllib.request, urllib.error, os

exe = r"D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"
data = tempfile.mkdtemp(prefix="mcp_dbg_")
PORT = 9124
cfg = {"mcp": {"enabled": True, "host": "127.0.0.1", "port": PORT, "token": "secret"}}
with open(os.path.join(data, "config.json"), "w", encoding="utf-8") as f:
    json.dump(cfg, f)
p = subprocess.Popen([exe, "--mcp-http", "--data-dir", data],
                     stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
time.sleep(1.5)
body = json.dumps({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}).encode()
req = urllib.request.Request(f"http://127.0.0.1:{PORT}/mcp", data=body, method="POST")
req.add_header("Content-Type","application/json")
req.add_header("Authorization","Bearer secret")
try:
    r = urllib.request.urlopen(req)
    print("status", r.status)
    print("headers", dict(r.headers))
    print("raw body:", repr(r.read()))
except urllib.error.HTTPError as e:
    print("HTTPError", e.code)
    print("headers", dict(e.headers))
    print("raw body:", repr(e.read()))
p.terminate()
