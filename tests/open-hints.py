"""A task that opens is told the other open tasks, what each holds and how to wait for one (`avp
wait`, the avp_wait tool), at most 8 of them: 25/10, agents polled the line instead of waiting.
Usage: python3 -I tests/open-hints.py <avp binary> <scratch dir>. Prints PASS/FAIL per check."""
import json, os, re, subprocess, sys, time

# A folder of its own inside the scratch dir: the scratch dir itself is never emptied (25/10, it was
# someone's working folder).
import tempfile
AVP, W = sys.argv[1], tempfile.mkdtemp(prefix="e2e.", dir=sys.argv[2])
PORT = "7407"
env = dict(os.environ, AVP_SERVER=f"http://127.0.0.1:{PORT}", AVP_REPO="hints", AVP_PERSON="tester", AVP_MCP_LOG="", AVP_VIEWS=f"{W}/views", AVP_SPECULATE="0")
env.pop("AVP_VIEW_IN_CHECKOUT", None)
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {detail}"))


run = lambda *a, **k: subprocess.run(a, check=True, capture_output=True, text=True, **k)


def open_task(name, *scope):
    os.makedirs(f"{W}/{name}")
    out = run(AVP, "open", "--goal", f"task {name}", *[a for p in scope for a in ("--scope", p)], "-C", f"{W}/{name}", env=env).stdout
    return re.search(r"task (\S+) opened", out).group(1), out


proj = f"{W}/proj"
os.makedirs(f"{proj}/src")
open(f"{proj}/src/a.txt", "w").write("a\n")
server = subprocess.Popen([AVP, "serve", "--listen", f"127.0.0.1:{PORT}", "--data", f"{W}/data"], stdout=open(f"{W}/server.log", "w"), stderr=subprocess.STDOUT)
time.sleep(1)
try:
    run(AVP, "init", "hints", cwd=proj, env=env)
    first, out = open_task("t0", "src/t0.txt", "src/t0.test.txt")
    check("a task alone hears of no other", "Other tasks are open" not in out, out)
    ids = [first] + [open_task(f"t{i}", f"src/t{i}.txt")[0] for i in range(1, 10)]
    _, out = open_task("me")
    check("open names another task with its goal and what it holds", f"\n    {first} task t0 (holds src/t0.txt, src/t0.test.txt)" in out, out)
    check("eight of the ten are named, the rest counted", sum(i in out for i in ids) == 8 and "and 2 more" in out, out)
    check("open says how to wait for one", "`avp wait --task <id>`" in out and "brings it into your folder" in out, out)

    os.makedirs(f"{W}/agent")
    mcp = subprocess.Popen([AVP, "mcp"], cwd=f"{W}/agent", env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    n = 0

    def call(method, params):
        global n
        n += 1
        mcp.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
        mcp.stdin.flush()
        while True:
            m = json.loads(mcp.stdout.readline())
            if m.get("id") == n:
                return m

    call("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e"}})
    r = call("tools/call", {"name": "avp_open", "arguments": {"goal": "task over mcp", "repo": "hints"}})
    out = "".join(c.get("text", "") for c in r["result"]["content"])
    check("avp_open names them too", f"\n  {first} task t0 (holds src/t0.txt, src/t0.test.txt)" in out and "and 3 more" in out, out)
    check("avp_open says the avp_wait tool waits for one", "the avp_wait tool" in out, out)
    mcp.stdin.close()
    mcp.wait(timeout=20)
finally:
    server.terminate()
print(f"open-hints PASS: {sum(results)}/{len(results)}")
