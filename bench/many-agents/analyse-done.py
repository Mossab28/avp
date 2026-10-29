"""The same-house bench, per run: did every section pass, after how many rounds, what it took (tokens and
dollars over all rounds, agent time), and the red states of the line. Usage: python3 -I analyse-done.py <run dir> [...]"""
import glob, json, os, re, sys
# $ per million tokens (input, cached input, output) of the models run through Codex; output holds the reasoning.
PRICES = {"gpt-5.6-luna": (0.20, 0.02, 1.20), "gpt-6-luna": (0.10, 0.01, 0.50), "gpt-6.1-terra": (1.0, 0.05, 5.0), "gpt-6.1-sol": (2.0, 0.10, 10.0)}
total_usd = total_tok = 0.0
for d in sys.argv[1:]:
    d = d.rstrip("/")
    log = open(f"{d}/log").read() if os.path.exists(f"{d}/log") else ""
    groups = {}
    try:
        for f in json.load(open(f"{d}/acceptance.json"))["testResults"]:
            for t in f["assertionResults"]:
                g = (t.get("ancestorTitles") or ["?"])[0]; ok, n = groups.get(g, (0, 0)); groups[g] = (ok + (t["status"] == "passed"), n + 1)
    except Exception:
        pass
    want = re.search(r"start: \w+, \d+ agents, (\d+) sections", log)
    states = [l.split() for l in open(f"{d}/states.txt") if l.strip()] if os.path.exists(f"{d}/states.txt") else []
    inp = cached = out = 0; oc_usd = 0.0; by_round = {}
    model = (open(f"{d}/model.txt").read().split() or ["gpt-5.6-luna"])[0] if os.path.exists(f"{d}/model.txt") else "gpt-5.6-luna"
    IN, CACHED, OUT = PRICES.get(model, PRICES["gpt-5.6-luna"])
    for f in glob.glob(f"{d}/agent*-r*.jsonl"):
        r = int(re.search(r"-r(\d+)\.jsonl$", f)[1])
        for line in open(f, errors="replace"):
            if '"step_finish"' in line:
                # OpenCode: tokens per step (input, cache reads and writes, output) and its own price.
                try: p = json.loads(line)["part"]
                except Exception: continue
                t = p.get("tokens") or {}; c = t.get("cache") or {}
                step_in = t.get("input", 0) + c.get("read", 0) + c.get("write", 0)
                inp += step_in; cached += c.get("read", 0); out += t.get("output", 0) + t.get("reasoning", 0); oc_usd += p.get("cost") or 0
                by_round[r] = by_round.get(r, 0) + step_in + t.get("output", 0) + t.get("reasoning", 0)
                continue
            if '"turn.completed"' not in line:
                continue
            u = json.loads(line).get("usage") or {}
            inp += u.get("input_tokens", 0); cached += u.get("cached_input_tokens", 0); out += u.get("output_tokens", 0)
            by_round[r] = by_round.get(r, 0) + u.get("input_tokens", 0) + u.get("output_tokens", 0)
    # Codex: the sessions' own cumulative counts are the truth (a turn cut at a time limit never writes its
    # usage to the transcript).
    sessions = glob.glob(f"{d}/codex-home/sessions/**/rollout-*.jsonl", recursive=True)
    if sessions and not oc_usd:
        si = sc = so = 0
        for f in sessions:
            last = None
            for line in open(f, errors="replace"):
                if '"total_token_usage"' in line:
                    last = line
            try:
                u = json.loads(last)["payload"]["info"]["total_token_usage"]
            except Exception:
                continue
            si += u.get("input_tokens", 0); sc += u.get("cached_input_tokens", 0); so += u.get("output_tokens", 0)
        if si + so > inp + out:
            inp, cached, out = si, sc, so
    usd = oc_usd if oc_usd else ((inp - cached) * IN + cached * CACHED + out * OUT) / 1e6; total_usd += usd; total_tok += inp + out
    ci = [l.split() for l in open(f"{d}/ci/runs.txt")] if os.path.exists(f"{d}/ci/runs.txt") else []
    fin = re.search(r"finished: (\d+) min (\d+) s of agent time over (\d+) rounds", log)
    print(json.dumps({"run": os.path.basename(d), "model": model, "agents": len(glob.glob(f"{d}/agent*-r1.jsonl")), "done": "done: every section passes" in log,
        "rounds": int(fin[3]) if fin else None, "minutes": round(int(fin[1]) + int(fin[2]) / 60, 1) if fin else None,
        "sections": f"{sum(ok == n for ok, n in groups.values())}/{want[1] if want else len(groups)}",
        "red_states": f"{sum(s[2] == 'red' for s in states)}/{len(states)}", "tokens_m": round((inp + out) / 1e6, 2), "usd": round(usd, 2),
        "tokens_m_by_round": [round(by_round[k] / 1e6, 2) for k in sorted(by_round)], "braked": os.path.exists(f"{d}/braked"),
        "ci_runs": len(ci), "ci_red": sum(c[2] == "red" for c in ci), "ci_minutes": round(sum(int(c[1]) for c in ci) / 60, 1)}))
print(json.dumps({"total_usd": round(total_usd, 2), "total_tokens_m": round(total_tok / 1e6, 1)}))
