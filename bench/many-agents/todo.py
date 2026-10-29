"""The sections not done on the line: those whose acceptance tests do not all pass (or did not run), one
line per queue, "<queue>:<section>, <section>". Usage: python3 -I todo.py <acceptance json> <sections.json>"""
import json, sys
groups = {}
try:
    for f in json.load(open(sys.argv[1]))["testResults"]:
        for t in f["assertionResults"]:
            g = (t.get("ancestorTitles") or ["?"])[0]; ok, n = groups.get(g, (0, 0)); groups[g] = (ok + (t["status"] == "passed"), n + 1)
except Exception:
    pass
todo = {}
for s in json.load(open(sys.argv[2])):
    ok, n = groups.get(s["describe"], (0, 0))
    if n == 0 or ok < n:
        todo.setdefault(s["queue"], []).append(s["section"])
for q in sorted(todo):
    print(f"{q}:{', '.join(todo[q])}")
