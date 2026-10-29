# Collect the flash_10 runs: per condition, tasks passed, cost, tokens, merge outcomes.
import json, glob, os, re, sys
ROOT = "/root/bench/CooperBench/logs"
PREFIX = sys.argv[1] if len(sys.argv) > 1 else "flash"
conds = {"solo": f"{PREFIX}-solo/solo", "git": f"{PREFIX}-git/coop", "avp": f"{PREFIX}-avp/coop"}
out = {}
for cond, sub in conds.items():
    rows = []
    for ev in sorted(glob.glob(f"{ROOT}/{sub}/*/*/*/eval.json")):
        d = os.path.dirname(ev)
        e = json.load(open(ev))
        r = {}
        try: r = json.load(open(f"{d}/result.json"))
        except Exception: pass
        cost = 0.0; inp = outp = cr = cw = 0
        agents = r.get("agents", {}) if isinstance(r.get("agents"), dict) else {}
        if agents:
            for a, v in agents.items():
                cost += float(v.get("cost") or 0)
        else:
            cost = float(r.get("cost") or r.get("total_cost") or 0)
        for sj in glob.glob(f"{d}/*_stream.jsonl"):
            for line in open(sj, errors="ignore"):
                if '"type":"result"' not in line and '"type": "result"' not in line: continue
                try: j = json.loads(line)
                except Exception: continue
                u = j.get("usage") or {}
                inp += u.get("input_tokens", 0); outp += u.get("output_tokens", 0)
                cr += u.get("cache_read_input_tokens", 0); cw += u.get("cache_creation_input_tokens", 0)
        mg = e.get("merge") or {}
        merge = f"{mg.get('status','-')}/{mg.get('strategy','-')}" if mg else "-"
        feats = {k: f"{'P' if v.get('passed') else 'F'}{v.get('tests_passed',0)}/{v.get('tests_failed',0)}" for k, v in e.items() if k.startswith("feature") and isinstance(v, dict)}
        both = e.get("both_passed")
        if both is None:
            fs = [v for k, v in e.items() if k.startswith("feature") and isinstance(v, dict)]
            both = all(f.get("passed") for f in fs) if fs else False
        rows.append({"task": f"{e.get('repo')}/{e.get('task_id')} {e.get('features')}", "passed": bool(both), "merge": merge,
                     "cost": round(cost, 3), "feats": feats, "in": inp, "out": outp, "cache_r": cr, "cache_w": cw, "error": (e.get("error") or "")[:80]})
    out[cond] = rows
print(json.dumps(out, indent=1))
for cond, rows in out.items():
    n = len(rows); p = sum(r["passed"] for r in rows)
    print(f"# {cond}: {p}/{n} passed, cost {sum(r['cost'] for r in rows):.2f}, out tokens {sum(r['out'] for r in rows)}, cache_r {sum(r['cache_r'] for r in rows)}, merges {dict((m, sum(1 for r in rows if r['merge']==m)) for m in set(r['merge'] for r in rows))}", file=sys.stderr)
