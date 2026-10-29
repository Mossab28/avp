"""A split step on real workers: the server queues one job per part, workers that know parts claim
them side by side, and the first red part stops the others on their workers.
Usage: python3 -I tests/split-workers.py <avp binary> <scratch dir>. Prints PASS/FAIL per check."""
import json, os, shutil, subprocess, sys, time, urllib.request

AVP, W = sys.argv[1], sys.argv[2]
shutil.rmtree(W, ignore_errors=True)
os.makedirs(W)
PORT = "7251"
SERVER = f"http://127.0.0.1:{PORT}"
env = dict(os.environ, AVP_SERVER=SERVER, AVP_PERSON="tester", AVP_MCP_LOG="")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {detail}"))


def api(path, body=None):
    req = urllib.request.Request(SERVER + path, data=None if body is None else json.dumps(body).encode(), headers={"content-type": "application/json"}, method="GET" if body is None else "POST")
    with urllib.request.urlopen(req, timeout=120) as r:
        return json.loads(r.read() or b"null")


GREEN = '[check]\n[[check.steps]]\nname = "lint"\nrun = "true"\n[[check.steps]]\nname = "tests"\nrun = "echo ran {shard}"\nsplit = 1\n'
RED = '[check]\n[[check.steps]]\nname = "lint"\nrun = "sleep 30"\n[[check.steps]]\nname = "tests"\nrun = "case {shard} in 2/3) echo boom; exit 1;; *) sleep 30;; esac"\nsplit = 1\n'
proj = f"{W}/proj"
os.makedirs(f"{proj}/src")
files = {"avp.toml": GREEN, "src/a.ts": "export const a = 1;\n"}
for name in ["one", "two", "three"]:
    files[f"src/{name}.test.ts"] = 'import { a } from "./a";\n'
for p, c in files.items():
    open(f"{proj}/{p}", "w").write(c)
run = lambda *a, **k: subprocess.run(a, check=True, capture_output=True, text=True, **k)
run("git", "init", "-q", "-b", "main", cwd=proj)
run("git", "add", "-A", cwd=proj)
run("git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "start", cwd=proj)
procs = [subprocess.Popen([AVP, "serve", "--listen", f"127.0.0.1:{PORT}", "--data", f"{W}/data"], env=dict(env, AVP_WORKERS="1"), stdout=open(f"{W}/server.log", "w"), stderr=subprocess.STDOUT)]
time.sleep(1)
try:
    run(AVP, "import-git", "split", cwd=proj, env=env)
    shutil.rmtree(f"{proj}/.avp")
    # Four workers, each with its own folder: the three shares and the other steps at once.
    for i in range(4):
        procs.append(subprocess.Popen([AVP, "worker", "--data", f"{W}/worker-{i}"], env=env, stdout=open(f"{W}/worker-{i}.log", "w"), stderr=subprocess.STDOUT))
    view = f"{W}/view"
    os.makedirs(view)
    run(AVP, "open", "--repo", "split", "--goal", "touch a", "-C", view, env=env)
    open(f"{view}/src/a.ts", "w").write("export const a = 2;\n")
    run(AVP, "sync", "-C", view, env=env)
    task = json.load(open(f"{view}/.avp/view.json"))["task"]
    proof = api(f"/repos/split/tasks/{task}/check", {})
    names = [s["name"] for s in proof["steps"]]
    check("green: the steps come back in avp.toml's order", proof["verdict"] == "green" and names == ["lint", "tests"], proof)
    tests = proof["steps"][1]["output"] if len(proof["steps"]) > 1 else ""
    check("green: three shares ran, one job each", all(f"ran {s}" in tests for s in ["1/3", "2/3", "3/3"]), tests)
    logs = "".join(open(f"{W}/worker-{i}.log").read() for i in range(4))
    check("the workers fetched the tree by batches", "blobs fetched" in logs, logs[:300])
    open(f"{view}/avp.toml", "w").write(RED)
    run(AVP, "sync", "-C", view, env=env)
    t = time.time()
    proof = api(f"/repos/split/tasks/{task}/check", {})
    took = time.time() - t
    check("red: the red share is the verdict", proof["verdict"] == "red" and [(s["name"], s["exit"]) for s in proof["steps"]] == [("tests", 1)], proof["steps"])
    check(f"red: it comes back without waiting for the others ({took:.1f}s)", took < 15)
    time.sleep(3)
    logs = "".join(open(f"{W}/worker-{i}.log").read() for i in range(4))
    check("the workers heard the others are moot and stopped", logs.count("cancelled by the server") >= 2, logs[-600:])
    depth = api("/work")
    check("nothing left running on the workers", depth["running"] == 0 and depth["waiting"] == 0, depth)
finally:
    for p in procs:
        p.terminate()
print(f"split PASS: {sum(results)}/{len(results)}")
