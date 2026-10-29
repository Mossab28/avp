"""`avp open` in a view that holds changes never sent: the new task takes them, merged with what the
line changed meanwhile, and `avp goal` says the task's goal better before it lands.
Usage: python3 -I tests/open-carry.py <avp binary> <scratch dir>. Prints PASS/FAIL per check."""
import os, shutil, subprocess, sys, time

# A folder of its own inside the scratch dir: the scratch dir itself is never emptied (25/10, it was
# someone's working folder).
import tempfile
AVP, W = sys.argv[1], tempfile.mkdtemp(prefix="e2e.", dir=sys.argv[2])
PORT = "7261"
env = dict(os.environ, AVP_SERVER=f"http://127.0.0.1:{PORT}", AVP_PERSON="tester", AVP_MCP_LOG="")
results = []


def check(name, ok, detail=""):
    results.append(ok)
    print(("PASS " if ok else "FAIL ") + name + ("" if ok else f": {detail}"))


run = lambda *a, **k: subprocess.run(a, check=True, capture_output=True, text=True, **k)
avp = lambda view, *a: run(AVP, *a, "-C", view, env=env).stdout
proj = f"{W}/proj"
os.makedirs(proj)
open(f"{proj}/a.txt", "w").write("1\n2\n3\n4\n5\n")
run("git", "init", "-q", "-b", "main", cwd=proj)
run("git", "add", "-A", cwd=proj)
run("git", "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "start", cwd=proj)
server = subprocess.Popen([AVP, "serve", "--listen", f"127.0.0.1:{PORT}", "--data", f"{W}/data"], stdout=open(f"{W}/server.log", "w"), stderr=subprocess.STDOUT)
time.sleep(1)
try:
    run(AVP, "import-git", "carry", cwd=proj, env=env)
    mine, other = f"{W}/mine", f"{W}/other"
    os.makedirs(mine)
    os.makedirs(other)
    run(AVP, "open", "--repo", "carry", "--goal", "first line", "-C", mine, env=env)
    open(f"{mine}/a.txt", "w").write("ONE\n2\n3\n4\n5\n")
    avp(mine, "sync")
    avp(mine, "land")
    # Written here after the land, never sent.
    open(f"{mine}/a.txt", "w").write("ONE\n2\n3\n4\nFIVE\n")
    open(f"{mine}/new.txt", "w").write("new\n")
    # Meanwhile the line changes another line of the same file.
    run(AVP, "open", "--repo", "carry", "--goal", "third line", "-C", other, env=env)
    open(f"{other}/a.txt", "w").write("ONE\n2\nTHREE\n4\n5\n")
    avp(other, "sync")
    avp(other, "land")
    out = avp(mine, "open", "--goal", "fifth line")
    check("open says the unsent changes are in the task", "your unsent changes are in it: 1 as they were, 1 merged with the line" in out, out)
    check("the edited file holds the line's change and mine", open(f"{mine}/a.txt").read() == "ONE\n2\nTHREE\n4\nFIVE\n", open(f"{mine}/a.txt").read())
    check("the new file is still there", open(f"{mine}/new.txt").read() == "new\n")
    status = avp(mine, "status")
    check("both went to the task as writes", "a.txt" in status and "new.txt" in status, status)
    avp(mine, "goal", "The fifth line and a new file")
    avp(mine, "land")
    log = avp(mine, "log")
    check("the corrected goal is the line's record", "The fifth line and a new file" in log.splitlines()[0], log)
finally:
    server.terminate()
print(f"carry PASS: {sum(results)}/{len(results)}")
