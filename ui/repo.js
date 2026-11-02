// A repo's pages: its code, the line and each position, blame and why, tasks, proofs, environments
// and settings. Read from the routes `avp log`, `avp show`, `avp blame`, `avp why` read.
"use strict";

const R = (repo) => `/repos/${repoPath(repo)}`;
const P = (repo) => `/r/${repoPath(repo)}`;
// A repo's pages: `/r/acme/shop/...` on AVP Cloud (a space, then its name), `/r/shop/...` on a team
// server. A second part that is a tab's name is the tab, not a name.
const REPO = String.raw`[^/]+(?:/(?!(?:code|history|line|p|tasks|proofs|envs|settings)(?:/|$))[^/]+)?`;
const repoPage = (rest) => new RegExp(`^/r/(${REPO})${rest}$`);
// `git clone` of the line, served by the server (read only: the line moves by landing).
const cloneUrl = (repo) => `${ORIGIN}/${repo}.git`;

// The tabs of every repo page, with the counts worth a glance.
function repoTabs(repo, on, counts = {}) {
  const tabs = [["code", "Code", ""], ["line", "Line", ""], ["tasks", "Tasks", counts.tasks], ["proofs", "Proofs", ""], ["envs", "Environments", ""], ["settings", "Settings", ""]];
  return `<nav class="tabs">${tabs.map(([k, label, n]) => `<a href="${P(repo)}${k === "code" ? "" : "/" + k}" class="${k === on ? "on" : ""}">${label}${n ? `<span class="n">${n}</span>` : ""}</a>`).join("")}</nav>`;
}
const OPEN = new Map();
// Open tasks are counted once per repo visit, for the tab.
async function openCount(repo) {
  if (!OPEN.has(repo)) OPEN.set(repo, api(`${R(repo)}/tasks?state=open`).then((t) => t.length).catch(() => 0));
  return OPEN.get(repo);
}
async function repoFrame(el, repo, tab, extra = []) {
  crumbs([[repo, P(repo)], ...extra]);
  const n = await openCount(repo);
  return repoTabs(repo, tab, { tasks: n });
}

// ---- trees: one fetch per position, an index of directories built once ----

const TREES = new Map();
// A tree of 20,000 files is one answer of a few megabytes, read once; listing a directory then
// reads its own entries only.
function treeAt(repo, position) {
  const key = `${repo}@${position}`;
  if (!TREES.has(key)) {
    TREES.set(key, api(`${R(repo)}/tree/${position}`).then((files) => {
      const dirs = new Map([["", { dirs: new Set(), files: [] }]]);
      const dir = (d) => { let x = dirs.get(d); if (!x) { x = { dirs: new Set(), files: [] }; dirs.set(d, x); } return x; };
      for (const path of Object.keys(files)) {
        const parts = path.split("/");
        let parent = "";
        for (let i = 0; i < parts.length - 1; i++) {
          const d = parent ? `${parent}/${parts[i]}` : parts[i];
          dir(parent).dirs.add(parts[i]);
          parent = d;
        }
        dir(parent).files.push(parts[parts.length - 1]);
      }
      return { files, dirs, count: Object.keys(files).length };
    }).catch((e) => { TREES.delete(key); throw e; }));
  }
  return TREES.get(key);
}

async function headOf(repo) {
  const h = await api(`${R(repo)}/positions?limit=1`);
  return h[0] || null;
}
const atQ = (at, head) => (at != null && at !== head ? `?at=${at}` : "");

function pathCrumbs(repo, path, at, head) {
  const parts = path ? path.split("/") : [];
  let acc = "";
  const links = [`<a href="${P(repo)}/code${atQ(at, head)}">${esc(repo)}</a>`];
  parts.forEach((p, i) => {
    acc = acc ? `${acc}/${p}` : p;
    links.push(i === parts.length - 1 ? `<strong>${esc(p)}</strong>` : `<a href="${P(repo)}/code/${acc.split("/").map(enc).join("/")}${atQ(at, head)}">${esc(p)}</a>`);
  });
  return `<div class="path">${links.join(" / ")}</div>`;
}
const codeHref = (repo, path, at, head) => `${P(repo)}/code/${path.split("/").map(enc).join("/")}${atQ(at, head)}`;

// ---- Code: the tree, a file, blame ----

async function code(el, [repo, rest], q, n) {
  const path = (rest || "").replace(/^\/+|\/+$/g, "");
  const tabs = await repoFrame(el, repo, "code");
  const head = await headOf(repo);
  if (!current(n)) return;
  if (!head) {
    el.innerHTML = tabs + empty("Nothing on the line yet", "Put the project's files on it from its folder:", setupCommands(repo));
    return;
  }
  const at = q.has("at") ? Number(q.get("at")) : head.position;
  const tree = await treeAt(repo, at);
  if (!current(n)) return;
  const pos = at === head.position ? head : (await api(`${R(repo)}/positions?from=${at}&limit=1`))[0];
  if (!current(n)) return;
  const atBar = `<div class="bar"><div class="grow">${pathCrumbs(repo, path, at, head.position)}</div>
    <span class="small soft">position <a href="${P(repo)}/p/${at}">${at}</a>${at !== head.position ? ` · <a href="${P(repo)}/code/${path}">see the head (${head.position})</a>` : ""}</span></div>`;
  if (path && tree.files[path]) return fileView(el, tabs, atBar, repo, path, tree.files[path], at, head.position, q.get("view") === "blame", n);
  const d = tree.dirs.get(path);
  if (!d) {
    el.innerHTML = tabs + atBar + failed(new ApiError(404, `No ${path} at position ${at}.`), false);
    return;
  }
  const rows = [
    ...[...d.dirs].sort().map((name) => `<a class="item" href="${codeHref(repo, path ? `${path}/${name}` : name, at, head.position)}"><span class="main"><span class="title">${esc(name)}/</span></span></a>`),
    ...d.files.sort().map((name) => `<a class="item" href="${codeHref(repo, path ? `${path}/${name}` : name, at, head.position)}"><span class="main"><span class="title mono">${esc(name)}</span></span></a>`),
  ];
  const readme = d.files.find((f) => /^readme(\.md|\.txt)?$/i.test(f));
  el.innerHTML = tabs + `<div class="split"><section>${atBar}
      <div class="list"><div class="item head"><span class="main"><span class="title">${pos ? esc(short(pos.note, 90)) : ""}</span><span class="meta">${pos ? `${esc(actorLine(pos.actor))} · ${ago(pos.landed_at)}` : ""}</span></span><span class="side">${plural(tree.count, "file")}</span></div>
      ${rows.join("") || `<div class="item soft">Empty directory</div>`}</div>
      <div id="readme"></div></section>
    <aside class="stack">${repoAside(repo, head)}</aside></div>`;
  if (readme) {
    const blob = tree.files[path ? `${path}/${readme}` : readme].blob;
    fetch(`${R(repo)}/objects/${blob}`, { headers: LIVE }).then((r) => r.ok ? r.text() : "").then((t) => {
      if (!current(n) || !t) return;
      const box = $("readme");
      if (box) box.innerHTML = `<div class="file" style="margin-top:var(--avp-space-4)"><div class="fhead"><strong>${esc(readme)}</strong></div><pre style="margin:0;padding:var(--avp-space-4);white-space:pre-wrap;font-size:var(--avp-size-sm)">${esc(t.slice(0, 20000))}</pre></div>`;
    }).catch(() => {});
  }
}

function repoAside(repo, head) {
  return `<div class="card"><h3>Clone</h3>${cmd(`git clone ${cloneUrl(repo)}`)}<p class="small soft">Read only: the line moves by landing tasks. To work on it:</p>${cmd(`avp open --repo ${repo} --goal "..."`)}</div>
    <div class="card"><h3>Head</h3><p><a href="${P(repo)}/p/${head.position}">Position ${head.position}</a> ${verdictPill(head.verdict)}</p><p class="small soft">${esc(short(head.note, 120))}</p><p class="small"><a href="${P(repo)}/line">The whole line</a> · <a href="/live?repo=${enc(repo)}">Live</a></p></div>`;
}

// Bigger than this, a file is not drawn: the raw bytes are a click away.
const DRAW_MAX = 2 * 1024 * 1024;
const FIRST_LINES = 5000;

async function fileView(el, tabs, atBar, repo, path, entry, at, head, blameOn, n) {
  const raw = `${R(repo)}/objects/${entry.blob}`;
  const r = await fetch(raw, { headers: LIVE });
  if (!r.ok) throw new ApiError(r.status, `${path}: ${r.status}`);
  const buf = new Uint8Array(await r.arrayBuffer());
  if (!current(n)) return;
  const binary = buf.subarray(0, 8000).includes(0);
  const toggle = `<a href="${codeHref(repo, path, at, head)}${blameOn ? "" : (atQ(at, head) ? "&" : "?") + "view=blame"}" class="button">${blameOn ? "Code" : "Blame"}</a>`;
  const fhead = `<div class="fhead"><span class="grow mono">${esc(path.split("/").pop())}</span><span class="soft">${(buf.length / 1024).toFixed(buf.length < 10240 ? 1 : 0)} KB</span>${binary ? "" : toggle}<a class="button" href="${P(repo)}/history/${path.split("/").map(enc).join("/")}">History</a><a class="button" href="${raw}" download="${esc(path.split("/").pop())}">Raw</a></div>`;
  if (binary || buf.length > DRAW_MAX) {
    el.innerHTML = tabs + atBar + `<div class="file">${fhead}${empty(binary ? "A binary file" : "Too big to show here", "Raw downloads it whole.")}</div>`;
    return;
  }
  const lines = new TextDecoder().decode(buf).split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  let blame = null;
  if (blameOn) {
    blame = await api(`${R(repo)}/history/blame?path=${enc(path)}&at=${at}`);
    if (!current(n)) return;
  }
  const draw = (upto) => {
    let prev = null;
    const rows = [];
    for (let i = 0; i < Math.min(upto, lines.length); i++) {
      let b = "";
      if (blame) {
        const bl = blame.lines[i];
        const same = bl && prev && bl.position === prev.position;
        b = `<td class="b">${bl && !same ? `<a href="${P(repo)}/p/${bl.position}" title="position ${bl.position}">${bl.position === "^" ? "^" : bl.position}</a> ${esc(bl.person || "")}` : ""}</td>`;
        rows.push(`<tr id="L${i + 1}"${bl && !same && i ? ` class="first"` : ""}><td class="n" data-line="${i + 1}">${i + 1}</td>${b}<td class="t">${esc(lines[i])}</td></tr>`);
        prev = bl;
      } else {
        rows.push(`<tr id="L${i + 1}"><td class="n" data-line="${i + 1}">${i + 1}</td><td class="t">${esc(lines[i])}</td></tr>`);
      }
    }
    return rows.join("");
  };
  el.innerHTML = tabs + atBar + `<div class="file">${fhead}<div id="why"></div><div class="code"><table id="lines">${draw(FIRST_LINES)}</table></div>
    ${lines.length > FIRST_LINES ? `<div class="state"><button id="more">Show the other ${(lines.length - FIRST_LINES).toLocaleString(LOCALE)} lines</button></div>` : ""}</div>`;
  $("more")?.addEventListener("click", (ev) => { $("lines").innerHTML = draw(lines.length); ev.target.parentElement.remove(); });
  // A line's number asks why it is what it is: the task that wrote it, its goal, its proof.
  const why = async (line) => {
    el.querySelectorAll("tr.on").forEach((t) => t.classList.remove("on"));
    document.getElementById(`L${line}`)?.classList.add("on");
    history.replaceState(null, "", `${location.pathname}${location.search}#L${line}`);
    const box = $("why");
    box.innerHTML = `<div class="why">${loading("Why line " + line)}</div>`;
    try {
      const w = await api(`${R(repo)}/why?path=${enc(path)}&line=${line}&at=${at}`);
      box.innerHTML = `<div class="why"><strong>Line ${line}</strong> ${w.position != null ? `comes from <a href="${P(repo)}/p/${w.position}">position ${w.position}</a>` : "has no known position"}${w.task ? `: ${esc(short(w.task.goal, 160))}<br><span class="small soft">${esc(actorLine(w.task.actor))} · task <span class="mono">${esc(w.task.id)}</span>${w.proof ? ` · proof <a href="${P(repo)}/proofs/${enc(w.proof.id)}">${esc(w.proof.verdict)}</a>` : ""}</span>` : w.position === 0 ? ": imported with the repo." : ""}</div>`;
    } catch (e) { box.innerHTML = `<div class="why">${esc(e.message)}</div>`; }
  };
  $("lines").addEventListener("click", (ev) => { const td = ev.target.closest("td.n"); if (td) why(Number(td.dataset.line)); });
  const m = location.hash.match(/^#L(\d+)$/);
  if (m) why(Number(m[1]));
}

page(repoPage(""), (el, [repo], q, n) => code(el, [repo, ""], q, n));
page(repoPage(String.raw`\/code(?:\/(.*))?`), (el, [repo, rest], q, n) => code(el, [repo, rest || ""], q, n));

// A file's history: the positions that changed it, newest first.
page(repoPage(String.raw`\/history\/(.+)`), async (el, [repo, path], q, n) => {
  const tabs = await repoFrame(el, repo, "code", [["history"]]);
  const from = q.get("from");
  const log = await api(`${R(repo)}/history/log?path=${enc(path)}&limit=50${from ? `&from=${from}` : ""}`);
  if (!current(n)) return;
  el.innerHTML = tabs + `<div class="bar"><div class="grow">${pathCrumbs(repo, path, null, null)}</div></div>` +
    (log.changes.length ? `<div class="list">${log.changes.map((c) => `<a class="item" href="${P(repo)}/p/${c.position}"><span class="pos">${c.position}</span><span class="main"><span class="title">${esc(short(c.goal, 110))}</span><span class="meta">${esc(c.change)} · ${esc(actorLine(c))} · ${ago(c.landed_at)}</span></span></a>`).join("")}</div>` : empty("No change found", "No position touched this path."))
    + (log.more != null ? `<p><a class="button" href="?from=${log.more}">Older</a></p>` : "");
});

// ---- the line ----

function positionItem(repo, p) {
  return `<a class="item" href="${P(repo)}/p/${p.position}"><span class="pos">${p.position}</span>
    <span class="main"><span class="title">${esc(short(p.note, 120))}</span><span class="meta">${esc(actorLine(p.actor)) || "imported"} · ${ago(p.landed_at)}${p.tags?.length ? ` · ${p.tags.map(esc).join(", ")}` : ""}</span></span>
    <span class="side">${p.proof ? `${verdictPill(p.verdict)}<br><span class="hide-sm">${dur(p.seconds)}</span>` : ""}</span></a>`;
}

page(repoPage(String.raw`\/line`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "line");
  const from = q.get("from");
  const ps = await api(`${R(repo)}/positions?limit=50${from ? `&from=${from}` : ""}`);
  if (!current(n)) return;
  if (!ps.length) {
    el.innerHTML = tabs + empty("The line is empty", "Each task landed here becomes a position, proven on the exact head first.", setupCommands(repo));
    return;
  }
  const last = ps[ps.length - 1].position;
  el.innerHTML = tabs + `<div class="bar"><h2 class="grow" style="margin:0">The line</h2><a class="button" href="/live?repo=${enc(repo)}">Watch it live</a></div>
    <div class="list">${ps.map((p) => positionItem(repo, p)).join("")}</div>
    <div class="bar" style="margin-top:var(--avp-space-4)">${from ? `<a class="button" href="${P(repo)}/line">Newest</a>` : ""}${last > 0 ? `<a class="button" href="?from=${last - 1}">Older</a>` : ""}</div>`;
});

// A diff as the server gives it (a unified patch per file), drawn line by line. A file of many
// lines starts folded, so a big position stays quick to open.
function diffView(files) {
  if (!files.length) return empty("No change", "This position changed no file.");
  const one = (f) => {
    const rows = [];
    let a = 0, b = 0;
    for (const l of String(f.patch || "").split("\n")) {
      if (l.startsWith("@@")) {
        const m = l.match(/-(\d+)(?:,\d+)? \+(\d+)/);
        if (m) { a = +m[1]; b = +m[2]; }
        rows.push(`<tr class="hunk"><td class="n"></td><td class="n"></td><td class="t">${esc(l)}</td></tr>`);
      } else if (l.startsWith("+")) rows.push(`<tr class="add"><td class="n"></td><td class="n">${b++}</td><td class="t">${esc(l.slice(1))}</td></tr>`);
      else if (l.startsWith("-")) rows.push(`<tr class="del"><td class="n">${a++}</td><td class="n"></td><td class="t">${esc(l.slice(1))}</td></tr>`);
      else if (l.startsWith("\\")) continue;
      else rows.push(`<tr><td class="n">${a++}</td><td class="n">${b++}</td><td class="t">${esc(l.slice(1))}</td></tr>`);
    }
    const body = f.binary ? `<p class="state">Binary file</p>` : `<div class="code"><table>${rows.join("")}</table></div>`;
    return `<details class="file" ${rows.length > 400 ? "" : "open"} style="margin-bottom:var(--avp-space-3)"><summary class="fhead"><span class="grow mono">${esc(f.path)}</span><span class="pill">${esc(f.change)}</span>${rows.length > 400 ? `<span class="soft small">${plural(rows.length, "line")}</span>` : ""}</summary>${body}</details>`;
  };
  return `<p class="soft small">${plural(files.length, "file")} changed</p>` + files.map(one).join("");
}

page(repoPage(String.raw`\/p\/(\d+)`), async (el, [repo, pos], q, n) => {
  const tabs = await repoFrame(el, repo, "line", [[`position ${pos}`]]);
  // Position 0 is where the line starts: it has files, no change.
  const [ps, diff, head] = await Promise.all([api(`${R(repo)}/positions?from=${pos}&limit=1`), pos === "0" ? [] : api(`${R(repo)}/line/${pos}/diff`), headOf(repo)]);
  if (!current(n)) return;
  const p = ps[0];
  if (!p || String(p.position) !== pos) throw new ApiError(404, `No position ${pos} on ${repo}.`);
  const took = p.opened_at ? dur((p.landed_at - p.opened_at) / 1000) : "";
  el.innerHTML = tabs + `
    <div class="bar"><h1 class="grow" style="margin:0">${esc(short(p.note, 200))}</h1>${p.position > 0 ? `<a class="button" href="${P(repo)}/p/${p.position - 1}">Previous</a>` : ""}${head && p.position < head.position ? `<a class="button" href="${P(repo)}/p/${p.position + 1}">Next</a>` : ""}</div>
    <div class="split"><section>${p.position === 0 ? empty("Where the line starts", `The files the repo began with. <a href="${P(repo)}/code?at=0">Browse them</a>.`) : diffView(diff)}</section>
    <aside class="stack"><div class="card"><dl class="facts">
      <dt>Position</dt><dd>${p.position}</dd>
      <dt>Person</dt><dd>${esc(p.actor?.person || "none")}</dd>
      <dt>Agent</dt><dd>${esc(p.actor ? agentName(p.actor.agent) : "none")}</dd>
      <dt>Landed</dt><dd>${when(p.landed_at)}${took ? `<br><span class="small soft">${took} after it opened</span>` : ""}</dd>
      <dt>Proof</dt><dd>${p.proof ? `<a href="${P(repo)}/proofs/${enc(p.proof)}">${esc(p.verdict || "proof")}</a> · ${dur(p.seconds)}` : "none"}</dd>
      ${p.full ? `<dt>Full suite</dt><dd><a href="${P(repo)}/proofs/${enc(p.full)}">its proof</a></dd>` : ""}
      ${p.base_position != null ? `<dt>Proven on</dt><dd>position ${p.base_position}</dd>` : ""}
      ${p.task ? `<dt>Task</dt><dd class="mono small">${esc(p.task)}</dd>` : ""}
      ${p.git ? `<dt>Git</dt><dd class="mono small">${esc(p.git.slice(0, 12))}</dd>` : ""}
      ${p.tags?.length ? `<dt>Tags</dt><dd>${p.tags.map((t) => `<span class="pill">${esc(t)}</span>`).join(" ")}</dd>` : ""}
    </dl></div>
    <div class="card"><h3>Browse</h3><p><a href="${P(repo)}/code?at=${p.position}">The code at this position</a></p>${cmd(`avp show ${p.position}`)}</div></aside></div>`;
});

// ---- tasks ----

page(repoPage(String.raw`\/tasks`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "tasks");
  const ts = await api(`${R(repo)}/tasks?state=open`);
  if (!current(n)) return;
  el.innerHTML = tabs + `<h2>Open tasks</h2>` + (ts.length
    ? `<div class="list">${ts.sort((a, b) => b.created_at - a.created_at).map((t) => `<a class="item" href="${P(repo)}/tasks/${enc(t.id)}"><span class="main"><span class="title">${esc(short(t.goal, 120))}</span><span class="meta">${esc(actorLine(t.actor))} · opened ${ago(t.created_at)} on position ${t.base_position}</span></span><span class="side hide-sm">${t.scope.slice(0, 3).map(esc).join(", ")}${t.scope.length > 3 ? "…" : ""}</span></a>`).join("")}</div>`
    : empty("No open task", "An agent opens one, writes, lands it: the line moves by tasks.", [`avp open --repo ${repo} --goal "what this change does"`]));
});

page(repoPage(String.raw`\/tasks\/([^/]+)`), async (el, [repo, id], q, n) => {
  const tabs = await repoFrame(el, repo, "tasks", [["task"]]);
  const [st, diff] = await Promise.all([api(`${R(repo)}/tasks/${enc(id)}`), api(`${R(repo)}/tasks/${enc(id)}/diff`).catch(() => [])]);
  if (!current(n)) return;
  const t = st.task;
  const lp = st.last_proof;
  el.innerHTML = tabs + `<div class="bar"><h1 class="grow" style="margin:0">${esc(short(t.goal, 200))}</h1><span class="pill solid">${esc(t.state)}</span></div>
    <div class="split"><section>${diffView(diff)}</section><aside class="stack"><div class="card"><dl class="facts">
      <dt>Person</dt><dd>${esc(t.actor.person)}</dd><dt>Agent</dt><dd>${esc(agentName(t.actor.agent))}</dd>
      <dt>Opened</dt><dd>${when(t.created_at)} on position ${t.base_position}${st.head_position != null ? ` (head ${st.head_position})` : ""}</dd>
      <dt>Writes</dt><dd>${st.writes.filter((w) => !w.undone).length}</dd>
      <dt>Holds</dt><dd class="small">${st.leases.map((l) => `<span class="mono">${esc(l.path)}</span>`).join("<br>") || "nothing"}</dd>
      <dt>Last proof</dt><dd>${lp ? `<a href="${P(repo)}/proofs/${enc(lp.id)}">${esc(lp.verdict)}</a>` : "not proven yet"}</dd>
      ${t.landed_position != null ? `<dt>Landed</dt><dd><a href="${P(repo)}/p/${t.landed_position}">position ${t.landed_position}</a></dd>` : ""}
      <dt>Id</dt><dd class="mono small">${esc(t.id)}</dd></dl></div>
      <div class="card"><h3>Wait for it</h3><p class="small soft">An agent that needs this work waits for it to land:</p>${cmd(`avp wait --task ${t.id}`)}</div></aside></div>`;
});

// ---- proofs ----

page(repoPage(String.raw`\/proofs`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "proofs");
  const from = q.get("from");
  const ps = (await api(`${R(repo)}/positions?limit=100${from ? `&from=${from}` : ""}`));
  if (!current(n)) return;
  const proven = ps.filter((p) => p.proof);
  const slowest = Math.max(1, ...proven.map((p) => p.seconds || 0));
  el.innerHTML = tabs + `<h2>Proofs of the line</h2><p class="soft small">Every landing is proven on the exact head of the line: these are the proofs that let each position land.</p>` + (proven.length
    ? `<div class="list steps">${proven.map((p) => `<a class="item" href="${P(repo)}/proofs/${enc(p.proof)}"><span class="pos">${p.position}</span><span class="main"><span class="title">${esc(short(p.note, 100))}</span><span class="meta">${plural(p.steps.length, "step")} · ${p.steps.filter((s) => s.exit !== 0).length ? "a red step" : "every step green"} · ${ago(p.landed_at)}</span></span>
      <span class="bar-t hide-sm"><i style="width:${(100 * (p.seconds || 0) / slowest).toFixed(1)}%"></i></span><span class="side">${verdictPill(p.verdict)}<br>${dur(p.seconds)}</span></a>`).join("")}</div>`
    : empty("No proof yet", "The first task that lands runs the checks of avp.toml on the head of the line."))
    + (ps.length === 100 ? `<p><a class="button" href="?from=${ps[ps.length - 1].position - 1}">Older</a></p>` : "");
});

page(repoPage(String.raw`\/proofs\/([^/]+)`), async (el, [repo, id], q, n) => {
  const tabs = await repoFrame(el, repo, "proofs", [["proof"]]);
  const p = await api(`${R(repo)}/proofs/${enc(id)}`);
  if (!current(n)) return;
  const total = p.steps.reduce((s, x) => s + (x.seconds || 0), 0);
  const longest = Math.max(0.001, ...p.steps.map((s) => s.seconds || 0));
  const t = p.timing || {};
  el.innerHTML = tabs + `<div class="bar"><h1 class="grow" style="margin:0">Proof ${verdictPill(p.verdict)}</h1><span class="soft small">${when(p.created_at)}</span></div>
    <div class="split"><section><div class="list steps">${p.steps.map((s, i) => `<div class="item" style="flex-wrap:wrap"><span class="dot ${s.exit === 0 ? "green" : "red"}"></span>
      <span class="main"><span class="title">${esc(s.name)}</span><span class="meta mono">${esc(s.command || "")}</span></span>
      <span class="bar-t"><i style="width:${(100 * (s.seconds || 0) / longest).toFixed(1)}%"></i></span><span class="side">${s.exit === 0 ? "" : `exit ${s.exit}<br>`}${dur(s.seconds)}</span>
      <button data-step="${i}" style="height:26px">Log</button><div class="log-box" style="flex-basis:100%"></div></div>`).join("")}</div></section>
    <aside class="stack"><div class="card"><dl class="facts">
      <dt>Verdict</dt><dd>${esc(p.verdict)}</dd>
      <dt>Steps</dt><dd>${dur(total)}</dd>
      ${t.total_s != null ? `<dt>End to end</dt><dd>${dur(t.total_s)}</dd><dt>Waiting</dt><dd>${dur(t.wait_s)}</dd><dt>Preparing</dt><dd>${dur(t.prep_s)}</dd>${t.setup_ran ? `<dt>Setup</dt><dd>${dur(t.setup_s)}</dd>` : ""}` : ""}
      <dt>Proven on</dt><dd>position ${p.base_position}</dd>
      <dt>Files</dt><dd class="small">${(p.files || []).slice(0, 12).map((f) => `<span class="mono">${esc(f)}</span>`).join("<br>")}${(p.files || []).length > 12 ? `<br>and ${p.files.length - 12} more` : ""}</dd>
      ${p.blamed?.length ? `<dt>Blamed</dt><dd class="small">${p.blamed.map(esc).join("<br>")}</dd>` : ""}
      <dt>Id</dt><dd class="mono small">${esc(p.id)}</dd></dl></div>${cmd(`avp proof ${p.id}`)}</aside></div>`;
  el.querySelectorAll("[data-step]").forEach((b) => b.addEventListener("click", async () => {
    const s = p.steps[+b.dataset.step];
    const box = b.parentElement.querySelector(".log-box");
    if (box.innerHTML) { box.innerHTML = ""; return; }
    box.innerHTML = loading("Log");
    try {
      const log = await api(`${R(repo)}/proofs/${enc(p.id)}/log?step=${enc(s.name)}`);
      const text = (log.lines || []).map((l) => (Array.isArray(l) ? l[1] : l.text ?? l)).join("\n") || s.output || "";
      box.innerHTML = `<pre class="log">${esc(text || "(no output)")}</pre>`;
    } catch { box.innerHTML = `<pre class="log">${esc(s.output || "(no output kept)")}</pre>`; }
  }));
});

// ---- environments and deploys ----

page(repoPage(String.raw`\/envs`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "envs");
  const [envs, cfg] = await Promise.all([api(`${R(repo)}/envs`), api(`${R(repo)}/config`).catch(() => ({ config: {} }))]);
  if (!current(n)) return;
  const conf = cfg.config?.envs || {};
  el.innerHTML = tabs + `<h2>Environments</h2>` + (envs.length
    ? `<div class="list">${envs.map((e) => {
        const c = conf[e.name] || {};
        const w = e.wanted, pr = e.progress;
        return `<div class="item" style="flex-wrap:wrap"><span class="main"><span class="title"><strong>${esc(e.name)}</strong>${c.after ? ` <span class="soft small">after ${esc(c.after)}</span>` : ""}</span>
          <span class="meta">${e.position >= 0 ? `serves <a href="${P(repo)}/p/${e.position}">position ${e.position}</a> since ${ago(e.moved_at)}` : "nothing known yet"}${w && w.position !== e.position ? ` · promotion to ${w.position} asked${w.by ? ` by ${esc(w.by)}` : ""}` : ""}</span>
          ${pr ? `<span class="meta">deploy: ${esc(pr.status || pr.state || "")} ${esc(pr.line || pr.message || "")}</span>` : ""}</span>
          <span class="side">${(c.promoters || c.approvers || []).length ? `promoters: ${(c.promoters || c.approvers).map(esc).join(", ")}` : "admins promote"}</span></div>`;
      }).join("")}</div>${cmd(`avp promote ${envs[0].name}`)}<p class="small soft">Promote and roll back from the CLI or an agent (<code>avp promote</code>, <code>avp rollback</code>): the site shows the line, it adds no verbs.</p>`
    : empty("No environment", "Say where the line deploys in avp.toml, and AVP promotes positions there:", ["[envs.staging]\ndeploy = { run = \"./deploy.sh staging\" }\n\n[envs.production]\nafter = \"staging\""]));
});

// ---- settings: roles, guards, secrets' names, the landing rule ----

page(repoPage(String.raw`\/settings`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "settings");
  const soft = (p) => p.catch((e) => ({ error: e }));
  const [info, roles, guarded, secrets, cfg] = await Promise.all([api(R(repo)), soft(api(`${R(repo)}/roles`)), soft(api(`${R(repo)}/guarded`)), soft(api(`${R(repo)}/secrets`)), soft(api(`${R(repo)}/config`))]);
  if (!current(n)) return;
  const or = (x, f) => (x && x.error ? `<p class="soft small">${esc(x.error.message)}</p>` : f(x));
  const steps = cfg.config?.check?.steps || [];
  el.innerHTML = tabs + `<div class="stack" style="max-width:820px">
    <section class="card"><h2>Repo</h2><dl class="facts"><dt>Name</dt><dd>${esc(fullName(info))}</dd><dt>Landing rule</dt><dd>${esc(info.landing)}${info.landing === "proof" ? " (a green proof lands)" : ""}</dd><dt>Policy</dt><dd>${esc(info.policy)}</dd><dt>Created</dt><dd>${when(info.created_at)}</dd><dt>Clone</dt><dd class="mono small">${esc(cloneUrl(repo))}</dd></dl></section>
    <section class="card"><h2>Roles</h2>${or(roles, (rs) => rs.length ? `<table><tr><th>Person</th><th>Role</th></tr>${rs.map((r) => `<tr><td>${esc(r.person)}</td><td><span class="pill">${esc(r.role)}</span></td></tr>`).join("")}</table>` : `<p class="soft">No roles: this server has no accounts, or nobody was given one.</p>`)}
      <form id="role" class="row" style="margin-top:var(--avp-space-3)"><input name="person" placeholder="person" required><select name="role" style="width:120px"><option>read</option><option>write</option><option>admin</option><option>none</option></select><button>Set role</button></form><p class="small" id="err"></p></section>
    <section class="card"><h2>Guarded paths</h2><p class="soft small">A change to these waits for a person's approval before it lands. ${esc((cfg.always_guarded || []).join(", "))} always do.</p>${or(guarded, (gs) => gs.length ? `<div class="list">${gs.map((g) => `<div class="item"><span class="main mono">${esc(g.pattern || g.path || g)}</span>${g.by ? `<span class="side">${esc(g.by)}</span>` : ""}</div>`).join("")}</div>` : `<p class="soft">Nothing else guarded.</p>`)}${cmd(`avp guard "deploy/**"`)}</section>
    <section class="card"><h2>Secrets</h2><p class="soft small">Their names only: values never leave the server, a proof's steps get the ones they name.</p>${or(secrets, (ss) => ss.length ? `<div class="list">${ss.map((s) => `<div class="item"><span class="main mono">${esc(s.name || s)}</span>${s.updated_at || s.set_at ? `<span class="side">set ${ago(s.updated_at || s.set_at)}</span>` : ""}</div>`).join("")}</div>` : `<p class="soft">No secret.</p>`)}${cmd(`avp secret set NAME`)}</section>
    <section class="card"><h2>Checks</h2><p class="soft small">What every landing runs on the head (${esc(cfg.file || "avp.toml")}, guarded).</p>${steps.length ? `<div class="list">${steps.map((s) => `<div class="item"><span class="main"><span class="title">${esc(s.name)}</span><span class="meta mono">${esc(s.run || "")}</span></span></div>`).join("")}</div>` : `<p class="soft">No check: every landing goes in unproven.</p>`}</section>
  </div>`;
  $("role").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    try { await api(`${R(repo)}/roles`, { method: "POST", body: { person: f.get("person").trim(), role: f.get("role") } }); render(); } catch (e) { $("err").textContent = e.message; }
  });
});
