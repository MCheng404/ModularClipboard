import subprocess, json, sys, tempfile, os

exe = r"D:\modular-clipboard\target\x86_64-pc-windows-msvc\debug\modular-clipboard.exe"
data = tempfile.mkdtemp(prefix="mcp_e2e_")
p = subprocess.Popen([exe, "--mcp-server", "--data-dir", data],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

def send(msg):
    p.stdin.write((json.dumps(msg) + "\n").encode())
    p.stdin.flush()
    line = p.stdout.readline()
    if not line:
        return None
    return json.loads(line.decode())

ok = True

# 1. initialize
r = send({"jsonrpc":"2.0","id":1,"method":"initialize",
          "params":{"protocolVersion":"2025-06-18","clientInfo":{"name":"e2e","version":"1"}}})
si = r["result"]["serverInfo"]
print("PASS initialize:", si["name"], si["version"])

# 2. notification -> no response
p.stdin.write(b'{"jsonrpc":"2.0","method":"notifications/initialized"}\n'); p.stdin.flush()

# 3. tools/list
r = send({"jsonrpc":"2.0","id":2,"method":"tools/list"})
names = [t["name"] for t in r["result"]["tools"]]
expect = ["screen_capture","click_element","type_text","scroll_view","resize_window",
          "window_focus","drag_drop","read_layout","key_press","clipboard"]
print(("PASS" if names == expect else "FAIL"), "tools/list:", names)
ok &= names == expect

# 4. tools/call screen_capture (real screenshot)
r = send({"jsonrpc":"2.0","id":3,"method":"tools/call",
          "params":{"name":"screen_capture","arguments":{}}})
content = r["result"]["content"]
if content and content[0]["type"] == "image" and len(content[0]["data"]) > 1000:
    print("PASS screen_capture: image base64 len", len(content[0]["data"]))
else:
    print("FAIL screen_capture:", r); ok = False

# 5. tools/call read_layout (no window -> all top-level windows)
r = send({"jsonrpc":"2.0","id":4,"method":"tools/call",
          "params":{"name":"read_layout","arguments":{}}})
txt = r["result"]["content"][0]["text"]
if "windows" in txt:
    print("PASS read_layout: returns windows list")
else:
    print("FAIL read_layout:", txt[:200]); ok = False

# 6. unknown method
r = send({"jsonrpc":"2.0","id":5,"method":"resources/list"})
if r["error"]["code"] == -32601:
    print("PASS unknown method -> -32601")
else:
    print("FAIL unknown method:", r); ok = False

# 7. clipboard 往返：write 后 read 必须拿到同一段文本
probe = "e2e-clipboard-probe-中文-42"
r = send({"jsonrpc":"2.0","id":6,"method":"tools/call",
          "params":{"name":"clipboard","arguments":{"action":"write","text":probe}}})
wrote = not r["result"].get("isError", False)
r = send({"jsonrpc":"2.0","id":7,"method":"tools/call",
          "params":{"name":"clipboard","arguments":{"action":"read"}}})
back = r["result"]["content"][0]["text"]
if wrote and probe in back:
    print("PASS clipboard write/read 往返")
else:
    print("FAIL clipboard:", r); ok = False

# 8. key_press 的非法输入必须返回 isError（而不是静默成功）。
#    这里刻意不测真实按键——那会真的往系统里注入按键，干扰使用者。
r = send({"jsonrpc":"2.0","id":8,"method":"tools/call",
          "params":{"name":"key_press","arguments":{"combo":"ctrl+nope"}}})
if r["result"].get("isError", False):
    print("PASS key_press 拒绝非法组合键")
else:
    print("FAIL key_press 应报错:", r); ok = False

p.terminate()
sys.exit(0 if ok else 1)
