"""What a run has spent so far, live: the last cumulative token count of each Codex session under the
given folder, at GPT-5.6 Luna's list prices. Usage: python3 -I live-usd.py <sessions dir>"""
import glob, json, sys
# $ per million tokens (input, cached input, output) of the models run through Codex; output holds the reasoning.
PRICES = {"gpt-5.6-luna": (0.20, 0.02, 1.20), "gpt-6-luna": (0.10, 0.01, 0.50), "gpt-6.1-terra": (1.0, 0.05, 5.0), "gpt-6.1-sol": (2.0, 0.10, 10.0)}
IN, CACHED, OUT = PRICES.get(sys.argv[2] if len(sys.argv) > 2 and sys.argv[1] != "--opencode" else "gpt-5.6-luna", PRICES["gpt-5.6-luna"])
inp = cached = out = 0
if sys.argv[1] == "--opencode":
    # OpenCode prices each step itself (its catalog's Bedrock prices): the sum of what the agents' steps cost.
    usd = 0.0
    for f in glob.glob(f"{sys.argv[2]}/agent*-r*.jsonl"):
        for line in open(f, errors="replace"):
            if '"step_finish"' in line:
                try: usd += json.loads(line)["part"].get("cost") or 0
                except Exception: pass
    print(round(usd, 3)); sys.exit(0)
for f in glob.glob(f"{sys.argv[1]}/**/rollout-*.jsonl", recursive=True):
    last = None
    for line in open(f, errors="replace"):
        if '"total_token_usage"' in line:
            last = line
    try:
        u = json.loads(last)["payload"]["info"]["total_token_usage"]
    except Exception:
        continue
    inp += u.get("input_tokens", 0); cached += u.get("cached_input_tokens", 0); out += u.get("output_tokens", 0)
print(round(((inp - cached) * IN + cached * CACHED + out * OUT) / 1e6, 3))
