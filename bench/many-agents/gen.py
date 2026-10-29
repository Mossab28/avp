"""Scales the Facturo XXL bench to N agents, keeping the work per agent: the 28 sections of XXL plus
generated business-suite modules (1.75 sections per agent in all), their hidden acceptance tests, and
the agents' queues. The modules are generated from one descriptor each, so a module's spec text and its
tests can never disagree. Smaller sizes are prefixes of bigger ones.
Usage: python3 gen.py <N> <XXL spec> <XXL acceptance tests> <XXL flow dir> <out dir>"""
import json, os, re, shutil, sys

DOMAINS = [
    ("Quotes|quote", "Leads|lead", "Opportunities|opportunity", "Deals|deal", "Sales reps|sales rep", "Territories|territory", "Commissions|commission"),
    ("Campaigns|campaign", "Coupons|coupon", "Referrals|referral", "Newsletters|newsletter", "Segments|segment", "Gift cards|gift card", "Promotions|promotion"),
    ("Tickets|ticket", "Ticket replies|ticket reply", "Service levels|service level", "Incidents|incident", "Knowledge articles|knowledge article", "Surveys|survey", "Escalations|escalation"),
    ("Warehouses|warehouse", "Shipments|shipment", "Carriers|carrier", "Parcels|parcel", "Returns|return", "Pick lists|pick list", "Delivery slots|delivery slot"),
    ("Purchase orders|purchase order", "Goods receipts|goods receipt", "Vendor quotes|vendor quote", "Requisitions|requisition", "Contracts|contract", "Vendor ratings|vendor rating", "Tenders|tender"),
    ("Employees|employee", "Departments|department", "Leave requests|leave request", "Payslips|payslip", "Benefits|benefit", "Trainings|training", "Job openings|job opening"),
    ("Vehicles|vehicle", "Fuel logs|fuel log", "Maintenance jobs|maintenance job", "Drivers|driver", "Trips|trip", "Tolls|toll", "Vehicle policies|vehicle policy"),
    ("Assets|asset", "Asset assignments|asset assignment", "Asset audits|asset audit", "Licenses|license", "Devices|device", "Repairs|repair", "Disposals|disposal"),
    ("Budgets|budget", "Budget lines|budget line", "Cost centers|cost center", "Fiscal years|fiscal year", "Accruals|accrual", "Write-offs|write-off", "Cash registers|cash register"),
    ("Cash forecasts|cash forecast", "Loans|loan", "Loan repayments|loan repayment", "Deposits|deposit", "Guarantees|guarantee", "Escrow accounts|escrow account", "Investments|investment"),
    ("Usage meters|usage meter", "Usage records|usage record", "Price plans|price plan", "Add-ons|add-on", "Seats|seat", "Entitlements|entitlement", "Trials|trial"),
    ("Dunning cases|dunning case", "Payment plans|payment plan", "Installments|installment", "Disputes|dispute", "Chargebacks|chargeback", "Collectors|collector", "Promises to pay|promise to pay"),
    ("Consents|consent", "Data requests|data request", "Retention rules|retention rule", "Policy acknowledgements|policy acknowledgement", "Risk assessments|risk assessment", "Compliance checks|compliance check", "Certifications|certification"),
    ("Milestones|milestone", "Deliverables|deliverable", "Change requests|change request", "Risks|risk", "Issues|issue", "Meetings|meeting", "Resource bookings|resource booking"),
    ("Venues|venue", "Events|event", "Registrations|registration", "Event passes|event pass", "Speakers|speaker", "Sponsors|sponsor", "Sessions|session"),
    ("Properties|property", "Units|unit", "Leases|lease", "Tenants|tenant", "Rent charges|rent charge", "Inspections|inspection", "Work requests|work request"),
    ("Work orders|work order", "Bills of materials|bill of materials", "Routings|routing", "Machines|machine", "Downtime logs|downtime log", "Quality checks|quality check", "Batches|batch"),
    ("Stores|store", "Shifts|shift", "Cash counts|cash count", "Price tags|price tag", "Stocktakes|stocktake", "Loyalty cards|loyalty card", "Layaways|layaway"),
    ("Matters|matter", "Engagements|engagement", "Legal documents|legal document", "Signatures|signature", "Deadlines|deadline", "Hearings|hearing", "Retainers|retainer"),
    ("Courses|course", "Enrollments|enrollment", "Instructors|instructor", "Classrooms|classroom", "Grades|grade", "Certificates|certificate", "Attendance sheets|attendance sheet"),
    ("Rooms|room", "Bookings|booking", "Housekeeping tasks|housekeeping task", "Minibar charges|minibar charge", "Guest folios|guest folio", "Rate plans|rate plan", "Room blocks|room block"),
    ("Fields|field", "Crops|crop", "Harvests|harvest", "Irrigation logs|irrigation log", "Equipment rentals|equipment rental", "Seed orders|seed order", "Yields|yield"),
    ("Channels|channel", "Episodes|episode", "Sponsorship slots|sponsorship slot", "Ad orders|ad order", "Royalties|royalty", "Media licenses|media license", "Playlists|playlist"),
    ("Energy sites|energy site", "Meter readings|meter reading", "Tariffs|tariff", "Outages|outage", "Solar panels|solar panel", "Feed-in credits|feed-in credit", "Grid fees|grid fee"),
    ("Sites|site", "Permits|permit", "Daily logs|daily log", "Subcontracts|subcontract", "Punch items|punch item", "Material deliveries|material delivery", "Safety incidents|safety incident"),
    ("Donors|donor", "Donations|donation", "Grants|grant", "Grant reports|grant report", "Volunteers|volunteer", "Volunteer shifts|volunteer shift", "Pledges|pledge"),
    ("Itineraries|itinerary", "Flights|flight", "Hotel stays|hotel stay", "Travel advances|travel advance", "Visa applications|visa application", "Per diems|per diem", "Travel approvals|travel approval"),
    ("Claims|claim", "Premiums|premium", "Adjusters|adjuster", "Coverages|coverage", "Endorsements|endorsement", "Renewals|renewal", "Underwriting notes|underwriting note"),
]
LABELS = ["name", "title", "label", "subject"]
# (field, min, max or None, valid value, two invalid values)
INTS = [("amountCents", 1, None, 1000, (0, 1.5)), ("quantity", 1, None, 3, (0, 2.5)), ("minutes", 1, 1440, 90, (0, 1441)),
        ("percent", 0, 100, 40, (-1, 101)), ("score", 1, 5, 4, (0, 6))]
ENUMS = [("priority", ["low", "normal", "high"]), ("channel", ["email", "phone", "web"]), ("kind", ["standard", "express", "special"]),
         ("region", ["north", "south", "east", "west"]), ("level", ["bronze", "silver", "gold"])]
LIFE = [("draft", "approve", "approved"), ("open", "close", "closed"), ("pending", "confirm", "confirmed"), ("planned", "complete", "completed"),
        ("active", "cancel", "cancelled"), ("new", "assign", "assigned"), ("requested", "accept", "accepted")]
CORE = {"customers": ("customer", "customerId"), "products": ("product", "productId"), "suppliers": ("supplier", "supplierId")}
TAKEN = {"cus", "inv", "pay", "crn", "sub", "whk", "key", "jrn", "sup", "pin", "spy", "exp", "bnk", "prj", "tim", "usr"}
WORDS = {16: "sixteen", 15: "fifteen", 32: "thirty-two", 31: "thirty-one", 64: "sixty-four", 63: "sixty-three",
         128: "a hundred and twenty-eight", 127: "a hundred and twenty-seven", 96: "ninety-six", 95: "ninety-five"}


def camel(words: str) -> str:
    parts = re.split(r"[\s-]+", words.lower())
    return parts[0] + "".join(p.capitalize() for p in parts[1:])


def modules():
    out, prefixes = [], set(TAKEN)
    for d, dom in enumerate(DOMAINS):
        names = [x.split("|") for x in dom]
        for i, (title, singular) in enumerate(names):
            m = len(out)
            label = LABELS[m % 4]
            num = INTS[(m * 3 + d) % 5]
            enum = ENUMS[(m + 2 * d) % 5]
            life = LIFE[(m + d) % 7]
            parent = {2: 0, 4: 1, 5: 3}.get(i)
            if i == 0:
                parent_ref, num = "customers", INTS[0]
            elif parent is not None:
                parent_ref = d * 7 + parent
            else:
                parent_ref = ["customers", "products", "suppliers"][(d + i) % 3]
            letters = re.sub(r"[^a-z]", "", title.lower())
            prefix = next(p for p in (letters[0] + a + b for k, a in enumerate(letters[1:], 1) for b in letters[k + 1:]) if p not in prefixes)
            prefixes.add(prefix)
            out.append({"title": title, "singular": singular, "path": "/" + re.sub(r"[\s]+", "-", title.lower()), "ident": camel(title),
                        "label": label, "num": num, "enum": enum, "date": m % 3 == 1, "life": life, "parent": parent_ref, "prefix": prefix,
                        "invoice": i == 0})
    for mod in out:
        p = mod["parent"]
        if isinstance(p, int):
            mod["parent_singular"], mod["parent_field"] = out[p]["singular"], camel(out[p]["singular"]) + "Id"
            mod["parent_ident"] = out[p]["ident"]
        else:
            mod["parent_singular"], mod["parent_field"] = CORE[p]
            mod["parent_ident"] = p
    return out


def article(word: str) -> str:
    return "an" if word[0] in "aeiou" else "a"


def spec_text(m) -> str:
    f, lo, hi, _, _ = m["num"]
    rule = f"a positive integer" if hi is None and lo == 1 else f"an integer from {lo} to {hi}"
    e, vals = m["enum"]
    ini, verb, fin = m["life"]
    fields = [m["parent_field"], m["label"], f, e] + (["date"] if m["date"] else [])
    date_rule = "; `date` a `YYYY-MM-DD` date" if m["date"] else ""
    s = (f"## {m['title']}\n\n"
         f"- `POST {m['path']}` `{{ {', '.join(fields)} }}` → 201 `{{ id, {', '.join(fields)}, status: \"{ini}\", createdAt }}`.\n"
         f"  The {m['parent_singular']} must exist (404); `{m['label']}` a non-empty string; `{f}` {rule}; `{e}` one of\n"
         f"  {', '.join(f'`{v}`' for v in vals)}{date_rule} (400 otherwise). Ids from `store.id(\"{m['prefix']}\")`.\n"
         f"- `GET {m['path']}?{m['parent_field']}=…&status=…` → 200, the filtered list sorted by `createdAt` then id.\n"
         f"  `GET {m['path']}/:id` → 200 or 404.\n"
         f"- `POST {m['path']}/:id/{verb}` → 200, the {m['singular']} with `status: \"{fin}\"`; 409 `{m['singular']} is not {ini}`\n"
         f"  unless it is {article(ini)} {ini} one; 404 for an unknown id.\n")
    if m["invoice"]:
        s += (f"- `POST {m['path']}/:id/invoice` → 201, a draft invoice for its customer with one line `{{ description: <{m['label']}>,\n"
              f"  quantity: 1, unitCents: <amountCents>, vatPercent: 20 }}` (the customer's discount applies as usual); 409\n"
              f"  `{m['singular']} already invoiced` the second time; 404 for an unknown id.\n")
    return s


def tests_text(mods) -> str:
    t = ["", f"// Business suite (generated): {len(mods)} modules, each tested from the descriptor its SPEC.md section was written from.",
         "type GC = ReturnType<typeof client>;", "let gSeq = 0;",
         "async function gcustomers(c: GC) { const n = ++gSeq; const r = await c.post(\"/customers\", { name: `Suite ${n}`, email: `s${n}@suite.io` }); expect(r.status).toBe(201); return r.body.id as string; }",
         "async function gproducts(c: GC) { const n = ++gSeq; const r = await c.post(\"/products\", { sku: `GS-${n}`, name: `Item ${n}`, unitCents: 500, vatPercent: 20 }); expect(r.status).toBe(201); return r.body.id as string; }",
         "async function gsuppliers(c: GC) { const n = ++gSeq; const r = await c.post(\"/suppliers\", { name: `Vendor ${n}`, email: `v${n}@suite.io` }); expect(r.status).toBe(201); return r.body.id as string; }"]
    for m in mods:
        x, path, lab, pf = m["ident"], m["path"], m["label"], m["parent_field"]
        f, lo, hi, good, bad = m["num"]
        e, vals = m["enum"]
        ini, verb, fin = m["life"]
        valid = f"{pf}: p, {lab}: \"A\", {f}: {good}, {e}: \"{vals[0]}\"" + (", date: \"2026-03-14\"" if m["date"] else "")
        t.append(f"const V_{x} = (p: string) => ({{ {valid} }});")
        if m["parent_ident"] in CORE:
            t.append(f"async function P_{x}(c: GC) {{ return g{m['parent_ident']}(c); }}")
        else:
            t.append(f"async function P_{x}(c: GC) {{ return (await M_{m['parent_ident']}(c)).id as string; }}")
        t.append(f"async function M_{x}(c: GC, p?: string, over: Record<string, unknown> = {{}}) {{ const r = await c.post(\"{path}\", {{ ...V_{x}(p ?? (await P_{x}(c))), ...over }}); expect(r.status).toBe(201); return r.body; }}")
        t.append(f"describe(\"{m['title'].lower()}\", () => {{")
        t.append(f"  it(\"1 creates as {ini} and lists in creation order\", async () => {{\n"
                 f"    const c = client();\n    const p = await P_{x}(c);\n    const a = await M_{x}(c, p, {{ {lab}: \"A\" }});\n"
                 f"    expect(typeof a.id).toBe(\"string\");\n    expect(a.status).toBe(\"{ini}\");\n    expect(a.{pf}).toBe(p);\n"
                 f"    expect(a.{f}).toBe({good});\n    expect(a.{e}).toBe(\"{vals[0]}\");\n"
                 f"    await M_{x}(c, p, {{ {lab}: \"B\" }});\n    const list = await c.get(\"{path}\");\n    expect(list.status).toBe(200);\n"
                 f"    expect(list.body.map((r: any) => r.{lab})).toEqual([\"A\", \"B\"]);\n  }});")
        checks = [f"{{ ...V_{x}(p), {lab}: \"\" }}", f"{{ ...V_{x}(p), {f}: {bad[0]} }}", f"{{ ...V_{x}(p), {f}: {bad[1]} }}", f"{{ ...V_{x}(p), {e}: \"other\" }}"]
        if m["date"]:
            checks.append(f"{{ ...V_{x}(p), date: \"2026/03/14\" }}")
        t.append(f"  it(\"2 refuses invalid input with 400 and an unknown {m['parent_singular']} with 404\", async () => {{\n"
                 f"    const c = client();\n    const p = await P_{x}(c);\n"
                 + "".join(f"    expect((await c.post(\"{path}\", {b})).status).toBe(400);\n" for b in checks)
                 + f"    expect((await c.post(\"{path}\", V_{x}(\"nope\"))).status).toBe(404);\n"
                 f"    expect((await c.get(\"{path}\")).body).toEqual([]);\n  }});")
        t.append(f"  it(\"3 moves to {fin} once, then answers 409\", async () => {{\n"
                 f"    const c = client();\n    const a = await M_{x}(c);\n    const r = await c.post(`{path}/${{a.id}}/{verb}`);\n"
                 f"    expect(r.status).toBe(200);\n    expect(r.body.status).toBe(\"{fin}\");\n"
                 f"    expect((await c.post(`{path}/${{a.id}}/{verb}`)).status).toBe(409);\n"
                 f"    const g = await c.get(`{path}/${{a.id}}`);\n    expect(g.status).toBe(200);\n    expect(g.body.status).toBe(\"{fin}\");\n"
                 f"    expect((await c.get(\"{path}/nope\")).status).toBe(404);\n    expect((await c.post(\"{path}/nope/{verb}\")).status).toBe(404);\n  }});")
        t.append(f"  it(\"4 filters by {pf} and status\", async () => {{\n"
                 f"    const c = client();\n    const p1 = await P_{x}(c);\n    const p2 = await P_{x}(c);\n"
                 f"    const a = await M_{x}(c, p1, {{ {lab}: \"A\" }});\n    await M_{x}(c, p2, {{ {lab}: \"B\" }});\n    await M_{x}(c, p1, {{ {lab}: \"C\" }});\n"
                 f"    await c.post(`{path}/${{a.id}}/{verb}`);\n"
                 f"    const names = async (q: string) => (await c.get(`{path}?${{q}}`)).body.map((r: any) => r.{lab});\n"
                 f"    expect(await names(`{pf}=${{p1}}`)).toEqual([\"A\", \"C\"]);\n"
                 f"    expect(await names(\"status={fin}\")).toEqual([\"A\"]);\n"
                 f"    expect(await names(`{pf}=${{p1}}&status={ini}`)).toEqual([\"C\"]);\n  }});")
        if m["invoice"]:
            t.append(f"  it(\"5 invoices once for its customer\", async () => {{\n"
                     f"    const c = client();\n    const p = await P_{x}(c);\n    const a = await M_{x}(c, p, {{ amountCents: 2500 }});\n"
                     f"    const r = await c.post(`{path}/${{a.id}}/invoice`);\n    expect(r.status).toBe(201);\n"
                     f"    expect(r.body.customerId).toBe(p);\n    expect(r.body.status).toBe(\"draft\");\n    expect(r.body.totalCents).toBe(3000);\n"
                     f"    expect((await c.post(`{path}/${{a.id}}/invoice`)).status).toBe(409);\n  }});")
        t.append("});")
    return "\n".join(t) + "\n"


START = re.compile(r"^(async function|function|const|let|type|describe\()")
DEFINES = re.compile(r"^(?:async function|function|const|let|type)\s+(\w+)")
# The XXL sections whose tests do not sit under their own lowercased title.
DESCRIBE_OF = {"Payment terms and reminders": "terms and reminders", "Suppliers": "suppliers and purchase invoices",
               "Purchase invoices": "suppliers and purchase invoices"}
TESTS_NOTE = ("Each section has its acceptance tests in `spec/tests/` (a `.txt` file, so that nothing runs them before\n"
              "their section exists); your queue names the file. When you start a section, copy that file into\n"
              "`test/` without the `.txt` (`spec/tests/customers.acceptance.test.ts.txt` becomes\n"
              "`test/customers.acceptance.test.ts`), changing nothing in it: the section is done when those tests\n"
              "pass on the line, and you deliver it with them. Never edit an acceptance test: they are compared with\n"
              "the originals at the end.\n")


def split_tests(text: str) -> dict:
    """One file per describe block: the imports, the helpers it needs (and the ones those need), the block."""
    lines = text.split("\n")
    i = 0
    while i < len(lines) and (lines[i].startswith("//") or lines[i].startswith("import ") or not lines[i].strip()):
        i += 1
    imports = [l for l in lines[:i] if l.startswith("import ")]
    blocks, cur, pending = [], [], []
    for line in lines[i:]:
        if line.startswith("//"):
            if cur:
                blocks.append(cur); cur = []
            pending.append(line)
        elif START.match(line):
            if cur:
                blocks.append(cur)
            cur, pending = pending + [line], []
        elif cur:
            cur.append(line)
    if cur:
        blocks.append(cur)
    helpers, describes = [], []
    for b in blocks:
        head = next(l for l in b if not l.startswith("//"))
        d = DEFINES.match(head)
        if d:
            helpers.append((d[1], "\n".join(b)))
        elif head.startswith("describe("):
            describes.append((re.match(r'describe\("([^"]+)"', head)[1], "\n".join(b)))
    out = {}
    for name, body in describes:
        need, todo = set(), [body]
        while todo:
            t = todo.pop()
            for h, hb in helpers:
                if h not in need and re.search(rf"\b{h}\b", t):
                    need.add(h); todo.append(hb)
        out[name] = "\n".join([f"// Acceptance tests of the \"{name}\" section. Copy this file into test/ as it is; never change it."]
                              + imports + [""] + [hb for h, hb in helpers if h in need] + [body]).rstrip() + "\n"
    return out


def slug(describe: str) -> str:
    return re.sub(r"\s+", "-", describe) + ".acceptance.test.ts"


def main():
    n, spec, accept, flow, out = int(sys.argv[1]), sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5]
    total = round(1.75 * n)
    mods = modules()[: total - 28]
    os.makedirs(f"{out}/flow", exist_ok=True)
    os.makedirs(f"{out}/spec-tests", exist_ok=True)
    s = open(spec).read()
    if mods:
        s = s.replace("Every domain ships its own tests under `test/`.\n",
                      f"Every domain ships its own tests under `test/`. After users and roles come {len(mods)} modules of the\n"
                      f"business suite, built the same way, each in its own module.\n", 1)
        s = s.rstrip("\n") + "\n\n" + "\n".join(spec_text(m) for m in mods)
    open(f"{out}/SPEC.md", "w").write(s)
    full = open(accept).read() + (tests_text(mods) if mods else "")
    open(f"{out}/acceptance.test.ts", "w").write(full)
    files = split_tests(full)
    for name, body in files.items():
        open(f"{out}/spec-tests/{slug(name)}.txt", "w").write(body)
    json.dump(mods, open(f"{out}/modules.json", "w"), indent=1)
    common = open(f"{flow}/common.md").read().replace("sixteen", WORDS[n]).replace("fifteen", WORDS[n - 1])
    anchor = "Keep every existing test passing.\n"
    assert common.count(anchor) == 1
    common = common.replace(anchor, anchor + TESTS_NOTE, 1)
    open(f"{out}/flow/common.md", "w").write(common)
    for v in ("vcs-avp.md", "vcs-git.md"):
        shutil.copy(f"{flow}/{v}", f"{out}/flow/{v}")
    queues = [[l[3:].strip() for l in open(f"{flow}/queue-{q}.md") if l.startswith("## ")] for q in range(1, 17)]
    doubles = len(mods) - (n - 16)
    k = 0
    for q in range(n - 16):
        size = 2 if q < doubles else 1
        queues.append([m["title"] for m in mods[k:k + size]])
        k += size
    assert k == len(mods), (k, len(mods))
    sections = []
    for q, titles in enumerate(queues, 1):
        body = "\nYour queue (sections of SPEC.md):\n"
        for t in titles:
            d = DESCRIBE_OF.get(t, t.lower())
            assert d in files, (t, d)
            body += f"\n## {t}\nThe \"{t}\" section of SPEC.md. Its acceptance tests: `spec/tests/{slug(d)}.txt`.\n"
            sections.append({"queue": q, "section": t, "describe": d, "file": slug(d)})
        open(f"{out}/flow/queue-{q}.md", "w").write(body)
    json.dump(sections, open(f"{out}/flow/sections.json", "w"), indent=1)
    print(json.dumps({"agents": n, "sections": 28 + len(mods), "generated": len(mods), "spec_lines": s.count("\n")}))


if __name__ == "__main__":
    main()
