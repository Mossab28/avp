"""A lead and its subagents share one `avp mcp`: each subagent opens a task of its own, in a folder of
its own, and passes `task`; with several open and no `task`, a tool names them and guesses none; a
land that proves for seconds does not hold another subagent's land; each task lands what it wrote,
and nothing of another's (28/10: the second avp_open was refused, and calls went one at a time).
Usage: python3 -I tests/subagents.py <avp binary> <scratch dir>. Prints PASS/FAIL per check."""
import json, os, re, subprocess, sys, tempfile, threading, time, urllib.request

AVP, W = sys.argv[1], tempfile.mkdtemp(prefix="sub.", dir=sys.argv[2])
PORT = "7492"
SERVER = f"http://127.0.0.1:{PORT}"
env = {k: v for k, v in os.environ.items() if not k.startswith(("AVP_", "TRAME_"))}
env.update(AVP_SERVER=SERVER, AVP_PERSON="lead", AVP_MCP_LOG="", AVP_VIEWS=f"{W}/views", AVP_SPECULATE="0")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {str(detail)[:600]}"))


proj = f"{W}/proj"
os.makedirs(proj)
open(f"{proj}/README.md", "w").write("shop\n")
# The proof of a candidate that holds slow.txt takes 4 s; the others, none.
open(f"{proj}/avp.toml", "w").write('[[check.steps]]\nname = "gate"\nrun = "if [ -f slow.txt ]; then sleep 4; fi; true"\n')
server = subprocess.Popen([AVP, "serve", "--listen", f"127.0.0.1:{PORT}", "--data", f"{W}/data"], stdout=open(f"{W}/server.log", "w"), stderr=subprocess.STDOUT)


class Mcp:
    """One `avp mcp`, as a host drives it: requests go out without waiting, answers come by id."""

    def __init__(self, cwd):
        self.p = subprocess.Popen([AVP, "mcp"], cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.n, self.got, self.order, self.cv = 0, {}, [], threading.Condition()
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.p.stdout:
            m = json.loads(line)
            if "id" in m:
                with self.cv:
                    self.got[m["id"]] = (m, time.monotonic())
                    self.order.append(m["id"])
                    self.cv.notify_all()

    def send(self, method, params):
        with self.cv:
            self.n += 1
            n = self.n
        self.p.stdin.write(json.dumps({"jsonrpc": "2.0", "id": n, "method": method, "params": params}) + "\n")
        self.p.stdin.flush()
        return n

    def wait(self, n, timeout=60):
        with self.cv:
            self.cv.wait_for(lambda: n in self.got, timeout)
            return self.got[n]

    def tool(self, name, **args):
        m, _ = self.wait(self.send("tools/call", {"name": name, "arguments": args}))
        r = m.get("result") or {}
        return (r.get("content") or [{}])[0].get("text", m.get("error", {}).get("message", "")), bool(r.get("isError", "error" in m))


def writes(task):
    st = json.load(urllib.request.urlopen(f"{SERVER}/repos/subs/tasks/{task}"))
    return sorted({w["path"] for w in st["writes"] if not w.get("undone")})


try:
    for _ in range(50):
        try:
            urllib.request.urlopen(f"{SERVER}/repos")
            break
        except OSError:
            time.sleep(0.1)
    subprocess.run([AVP, "init", "subs"], cwd=proj, env=env, check=True, capture_output=True)
    os.makedirs(f"{W}/lead")
    lead = Mcp(f"{W}/lead")
    lead.wait(lead.send("initialize", {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "claude-code"}}))
    listed = lead.wait(lead.send("tools/list", {}))[0]["result"]["tools"]
    check("every tool but avp_open takes `task`", all(("task" in t["inputSchema"]["properties"]) == (t["name"] not in ("avp_open", "avp_migrate_report")) for t in listed))

    # Three subagents of one lead, each opening its own task through the lead's server.
    opened = {}
    for name in ("a", "b", "c"):
        text, err = lead.tool("avp_open", goal=f"piece {name}", repo="subs", scope=[f"{name}.txt"])
        m = re.search(r"task (\S+) opened", text)
        folder = re.search(r"Your folder: (\S+)", text)
        opened[name] = (m and m.group(1), folder and folder.group(1), text, err)
    check("a second and a third avp_open each open a task, none refused", all(t and not e for t, _, _, e in opened.values()), opened)
    folders = {f for _, f, _, _ in opened.values()}
    check("each task has a folder of its own", len(folders) == 3 and None not in folders, folders)
    check("the answer says to pass `task` once several are open", f'pass task "{opened["c"][0]}"' in opened["c"][2] and "3 tasks open" in opened["c"][2], opened["c"][2])
    ta, tb, tc = (opened[k][0] for k in "abc")

    for name, files in (("a", ["a.txt", "slow.txt"]), ("b", ["b.txt"]), ("c", ["c.txt"])):
        for f in files:
            open(os.path.join(opened[name][1], f), "w").write(f"{name}\n")
        # What a subagent's tests leave, with no .gitignore to hide it: never sent.
        os.makedirs(os.path.join(opened[name][1], "__pycache__"), exist_ok=True)
        open(os.path.join(opened[name][1], "__pycache__", "m.cpython-314.pyc"), "wb").write(name.encode())

    text, err = lead.tool("avp_status")
    check("with several open and no `task`, a tool guesses none: it names them", err and "3 tasks are open" in text and all(t in text for t in (ta, tb, tc)), text)
    text, err = lead.tool("avp_status", task=tb[:22])
    check("`task` (the start of an id) picks the task, and the status is that task's", not err and tb in text and "+ b.txt" in text and "a.txt" not in text and "c.txt" not in text, text)
    text, err = lead.tool("avp_status", task="task_nothing")
    check("a `task` this session has not opened is refused, with the list", err and "no open task task_nothing" in text and tc in text, text)

    # A's proof takes 4 s; B lands meanwhile, without waiting for it.
    na = lead.send("tools/call", {"name": "avp_land", "arguments": {"task": ta}})
    time.sleep(0.5)
    t0 = time.monotonic()
    nb = lead.send("tools/call", {"name": "avp_land", "arguments": {"task": tb}})
    mb, at_b = lead.wait(nb)
    ma, at_a = lead.wait(na)
    land_a, land_b = (m["result"]["content"][0]["text"] for m in (ma, mb))
    check("a subagent's land does not wait for another's long proof", "landed at position" in land_b and at_b < at_a and at_b - t0 < 3, (land_b, at_b - t0, at_a - t0))
    check("the long one lands too", "landed at position" in land_a, land_a)
    text, err = lead.tool("avp_land")
    check("with one task left open, no `task` is needed: it lands", not err and "landed at position" in text, text)
    check("each task landed what it wrote, nothing of another's, nothing its tests left", (writes(ta), writes(tb), writes(tc)) == (["a.txt", "slow.txt"], ["b.txt"], ["c.txt"]), (writes(ta), writes(tb), writes(tc)))
    head = json.load(urllib.request.urlopen(f"{SERVER}/repos/subs/line?limit=5"))
    check("the line holds the three positions", [e["position"] for e in head][:3] == [3, 2, 1], head)
    text, err = lead.tool("avp_log", task=ta)
    check("a read with the id of a task landed since still answers", not err and "piece a" in text, text)
    lead.p.stdin.close()
    lead.p.wait(timeout=15)
    check("the server ends when its host closes it", lead.p.returncode == 0, lead.p.returncode)
finally:
    server.kill()

sys.exit(0 if all(results) else 1)
