"""Agents' folders through real MCP sessions: a free folder is taken again by the next session with
its install, made the head again first; a session holding its folder keeps it from the others.
Usage: python3 -I tests/views-e2e.py <avp binary> <scratch dir>. Prints PASS/FAIL per check."""
import json, os, re, shutil, subprocess, sys, time

# A folder of its own inside the scratch dir: the scratch dir itself is never emptied (25/10, it was
# someone's working folder).
import tempfile
AVP, W = sys.argv[1], tempfile.mkdtemp(prefix="e2e.", dir=sys.argv[2])
PORT = "7241"
SERVER = f"http://127.0.0.1:{PORT}"
env = dict(os.environ, AVP_SERVER=SERVER, AVP_VIEWS=f"{W}/views", AVP_PERSON="tester", AVP_MCP_LOG="")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {detail}"))


# A project of 600 files, so that a layout takes several batches.
proj = f"{W}/proj"
os.makedirs(f"{proj}/src")
for i in range(600):
    open(f"{proj}/src/f{i}.txt", "w").write(f"file {i}\n")
open(f"{proj}/.gitignore", "w").write("node_modules/\n")
open(f"{proj}/src/a.txt", "w").write("a\n")
open(f"{proj}/src/b.txt", "w").write("b\n")
run = lambda *a, **k: subprocess.run(a, check=True, capture_output=True, text=True, **k)
run("git", "init", "-q", "-b", "main", cwd=proj)
run("git", "add", "-A", cwd=proj)
run("git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "start", cwd=proj)
server = subprocess.Popen([AVP, "serve", "--listen", f"127.0.0.1:{PORT}", "--data", f"{W}/data"], stdout=open(f"{W}/server.log", "w"), stderr=subprocess.STDOUT)
time.sleep(1)
try:
    run(AVP, "import-git", "vp", cwd=proj, env=env)
    # A plain checkout again, as after `avp up`: the agents start there, not in a view.
    shutil.rmtree(f"{proj}/.avp")

    class Session:
        def __init__(self, cwd):
            self.p = subprocess.Popen([AVP, "mcp"], cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            self.n = 0
            self.call_raw("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "e2e"}})

        def call_raw(self, method, params):
            self.n += 1
            self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.n, "method": method, "params": params}) + "\n")
            self.p.stdin.flush()
            while True:
                m = json.loads(self.p.stdout.readline())
                if m.get("id") == self.n:
                    return m

        def tool(self, name, args):
            r = self.call_raw("tools/call", {"name": name, "arguments": args})
            return "".join(c.get("text", "") for c in r["result"]["content"])

        def open(self, goal):
            out = self.tool("avp_open", {"goal": goal, "repo": "vp"})
            m = re.search(r"Your folder: (\S+)", out)
            assert m, out
            return m.group(1), out

        def end(self):
            self.p.stdin.close()
            self.p.wait(timeout=20)

    a = Session(proj)
    t0 = time.time()
    da, out = a.open("change a")
    check("a first layout of 600 files", "603 files laid out" in out or "604 files laid out" in out, out[:120])
    print(f"     first layout in {time.time() - t0:.2f}s")
    os.makedirs(f"{da}/node_modules/dep")
    open(f"{da}/node_modules/dep/index.js", "w").write("module.exports = 1\n")
    open(f"{da}/src/a.txt", "w").write("a changed\n")
    landed = a.tool("avp_land", {})
    check("session A lands", "landed at position" in landed, landed[:200])
    a.end()

    # Left behind in A's folder: a file no task landed, a tracked file changed and never sent.
    open(f"{da}/stray.txt", "w").write("not landed\n")
    open(f"{da}/src/b.txt", "w").write("b, never sent\n")

    b = Session(W)
    db, out = b.open("change b")
    check("session B takes A's free folder", db == da, f"{db} vs {da}")
    check("the install is still there", os.path.exists(f"{db}/node_modules/dep/index.js"))
    check("A's landed change is in it", open(f"{db}/src/a.txt").read() == "a changed\n")
    check("the file no task landed is gone", not os.path.exists(f"{db}/stray.txt"))
    check("the change never sent is undone", open(f"{db}/src/b.txt").read() == "b\n")

    c = Session(proj)
    dc, out = c.open("change c")
    check("session C, while B holds its folder, gets another", dc != db, dc)
    check("C's folder holds the head", open(f"{dc}/src/a.txt").read() == "a changed\n" and len(os.listdir(f"{dc}/src")) == 602)

    open(f"{db}/src/b.txt", "w").write("b changed\n")
    open(f"{dc}/src/c.txt", "w").write("c\n")
    out = b.tool("avp_land", {})
    check("session B lands", "landed at position" in out, out[:300])
    out = c.tool("avp_land", {})
    check("session C lands", "landed at position" in out, out[:300])
    # Within a session the folder stays: B's next task is in the same folder.
    db2, _ = b.open("change b again")
    check("B's next task stays in its folder", db2 == db)
    check("and sees C's landing", os.path.exists(f"{db2}/src/c.txt"))
    b.end()
    c.end()
finally:
    server.terminate()
print(f"views PASS: {sum(results)}/{len(results)}")
