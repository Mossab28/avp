// A repo's pages: its code, the line and each position, blame and why, tasks, proofs, environments
// and settings. Read from the routes `avp log`, `avp show`, `avp blame`, `avp why` read.
"use strict";

const R = (repo) => `/repos/${repoPath(repo)}`;
const P = (repo) => `/r/${repoPath(repo)}`;
// A repo's pages: `/r/acme/shop/...` on AVP Cloud (a space, then its name), `/r/shop/...` on a team
// server. A second part that is a tab's name is the tab, not a name.
const REPO = String.raw`[^/]+(?:/(?!(?:code|history|line|p|tasks|proofs|envs|activity|settings)(?:/|$))[^/]+)?`;
const repoPage = (rest) => new RegExp(`^/r/(${REPO})${rest}$`);
// `git clone` of the line, served by the server (read only: the line moves by landing).
const cloneUrl = (repo) => `${ORIGIN}/${repo}.git`;

// The repo's navigation, on the left of every page of it (a strip that scrolls at phone width),
// with the counts worth a glance.
const SECTIONS = [["code", "Code", "code"], ["line", "History", "history"], ["tasks", "Tasks", "tasks"], ["proofs", "Proofs", "proofs"], ["envs", "Environments", "envs"], ["activity", "Activity", "activity"], ["settings", "Settings", "settings"]];
function drawSide(repo, on, counts = {}) {
  const about = ABOUT_NOW.get(repo) || {};
  const name = (about.name || repo).split("/").pop();
  const side = $("side");
  side.innerHTML = `<div class="who-repo">${avatar(name, "sq lg")}<span class="main"><strong title="${esc(repo)}">${esc(name)}</strong><span class="soft">${about.visibility === "public" ? "Public" : "Private"}${about.archived ? " · archived" : ""}</span></span></div>`
    + SECTIONS.map(([k, label, ic]) => {
      const n = k === "line" ? counts.positions : k === "tasks" ? counts.tasks : null;
      const mark = n ? `<span class="count">${esc(n)}</span>` : k === "activity" ? `<span class="live-dot" aria-label="live"></span>` : "";
      return `<a class="nav${k === on ? " on" : ""}" href="${P(repo)}${k === "code" ? "" : "/" + k}"${k === on ? ` aria-current="page"` : ""}>${icon(ic)}${label}${mark}</a>`;
    }).join("");
  side.hidden = false;
  $("shell").classList.add("with-side");
  side.querySelector("a.on")?.scrollIntoView({ block: "nearest", inline: "nearest" });
}
const OPEN = new Map();
const LENGTH = new Map();
const ABOUT = new Map();
const ABOUT_NOW = new Map();
// Its description, visibility and whether it is archived, read once per repo visit (a server
// without the route: a repo like any other).
async function aboutOf(repo) {
  if (!ABOUT.has(repo)) ABOUT.set(repo, maybe(`${R(repo)}/about`).then((a) => a || {}).catch(() => ({})).then((a) => (ABOUT_NOW.set(repo, a), a)));
  return ABOUT.get(repo);
}
// Open tasks and the line's positions are counted once per repo visit, for the tabs.
async function openCount(repo) {
  if (!OPEN.has(repo)) OPEN.set(repo, api(`${R(repo)}/tasks?state=open`).then((t) => t.length).catch(() => 0));
  return OPEN.get(repo);
}
async function lineLength(repo) {
  if (!LENGTH.has(repo)) LENGTH.set(repo, api(`${R(repo)}/positions?limit=1`).then((p) => (p[0] ? p[0].position + 1 : 0)).catch(() => 0));
  return LENGTH.get(repo);
}
async function repoFrame(el, repo, tab, extra = []) {
  repoCrumbs(repo, extra);
  if ($("side").hidden || $("side").dataset.repo !== repo) { $("side").dataset.repo = repo; drawSide(repo, tab); }
  const [n, positions, about] = await Promise.all([openCount(repo), lineLength(repo), aboutOf(repo)]);
  // Reached by a name it had: the address and the headings say the one it has now.
  if (about.name && about.name !== repo && location.pathname.startsWith(P(repo))) history.replaceState(null, "", location.pathname.replace(P(repo), P(about.name)) + location.search + location.hash);
  drawSide(repo, tab, { tasks: n, positions: positions ? positions.toLocaleString(LOCALE) : "" });
  // Archived: read-only, said on every page of it; its settings say who and how to take it back.
  return about.archived && tab !== "settings" ? `<div class="notice"><strong>Archived, read-only</strong>${about.archived.at ? ` since ${when(about.archived.at)}` : ""}: nothing opens nor lands on it. An admin of it unarchives it in its settings.</div>` : "";
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
  const links = [`<a href="${P(repo)}${atQ(at, head)}">${esc(repo)}</a>`];
  parts.forEach((p, i) => {
    acc = acc ? `${acc}/${p}` : p;
    links.push(i === parts.length - 1 ? `<strong>${esc(p)}</strong>` : `<a href="${P(repo)}/code/${acc.split("/").map(enc).join("/")}${atQ(at, head)}">${esc(p)}</a>`);
  });
  return `<div class="path">${links.join(" / ")}</div>`;
}
const codeHref = (repo, path, at, head) => `${P(repo)}/code/${path.split("/").map(enc).join("/")}${atQ(at, head)}`;

// ---- a README, read: plain Markdown, never its HTML ----

// Markdown as a README writes it: headings, paragraphs, lists, quotes, code, tables, links and
// emphasis. Every character of the source is escaped first; only the tags drawn here exist, so a
// README that holds markup shows it as text. `link` says where a relative link or image goes.
function markdown(src, link) {
  const safe = (url) => /^(https?:|mailto:|#)/i.test(url) ? url : /^[a-z][a-z0-9+.-]*:/i.test(url) ? null : link(url);
  const emph = (s) => s.replace(/\*\*(.+?)\*\*|__(.+?)__/g, (_, a, b) => `<strong>${a ?? b}</strong>`)
    .replace(/(^|[^\w*])\*(?!\s)(.+?)\*(?!\w)|(^|\W)_(?!\s)(.+?)_(?!\w)/g, (_, p, a, q, b) => `${p ?? q}<em>${a ?? b}</em>`)
    .replace(/~~(.+?)~~/g, "<del>$1</del>");
  const text = (s) => emph(esc(s)).replace(/(^|[\s(])(https?:\/\/[^\s<)]+)/g, (_, p, u) => `${p}<a href="${u}">${u}</a>`);
  const inline = (s) => s.split(/(`+[^`]*?`+)/).map((part, i) => {
    if (i % 2) return `<code>${esc(part.replace(/^`+|`+$/g, ""))}</code>`;
    let out = "", at = 0;
    for (const m of part.matchAll(/(!?)\[([^\]]*)\]\(\s*<?([^)\s>]+)>?(?:\s+"[^"]*")?\s*\)/g)) {
      out += text(part.slice(at, m.index));
      const url = safe(m[3]);
      out += m[1] && url?.img ? `<img src="${esc(url.img)}" alt="${esc(m[2])}">` : url ? `<a href="${esc(url.href ?? url)}">${emph(esc(m[2])) || esc(m[3])}</a>` : text(m[2]);
      at = m.index + m[0].length;
    }
    return out + text(part.slice(at));
  }).join("");
  const slug = (s) => s.toLowerCase().replace(/<[^>]+>/g, "").replace(/&[a-z#0-9]+;/g, "").replace(/[^\w\s-]/g, "").trim().replace(/\s+/g, "-");
  const block = (lines) => {
    const out = [];
    for (let i = 0; i < lines.length;) {
      const l = lines[i];
      let m;
      if (!l.trim()) { i++; continue; }
      if ((m = l.match(/^\s*(```+|~~~+)\s*([\w+-]*)/))) {
        const fence = m[1], code = [];
        for (i++; i < lines.length && !lines[i].trimStart().startsWith(fence); i++) code.push(lines[i]);
        i++;
        out.push(`<pre><code>${esc(code.join("\n"))}</code></pre>`);
      } else if ((m = l.match(/^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/))) {
        const h = inline(m[2]);
        out.push(`<h${m[1].length} id="${slug(h)}">${h}</h${m[1].length}>`);
        i++;
      } else if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(l)) {
        out.push("<hr>");
        i++;
      } else if (/^\s*>/.test(l)) {
        const q = [];
        for (; i < lines.length && /^\s*>/.test(lines[i]); i++) q.push(lines[i].replace(/^\s*>\s?/, ""));
        out.push(`<blockquote>${block(q)}</blockquote>`);
      } else if (/^\s*\|.*\|\s*$/.test(l) && /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/.test(lines[i + 1] || "")) {
        const cells = (r) => r.trim().replace(/^\||\|$/g, "").split("|").map((c) => inline(c.trim()));
        const head = cells(l);
        const rows = [];
        for (i += 2; i < lines.length && /^\s*\|/.test(lines[i]); i++) rows.push(cells(lines[i]));
        out.push(`<div class="scroll-x"><table><tr>${head.map((c) => `<th>${c}</th>`).join("")}</tr>${rows.map((r) => `<tr>${r.map((c) => `<td>${c}</td>`).join("")}</tr>`).join("")}</table></div>`);
      } else if ((m = l.match(/^(\s*)([-*+]|\d+[.)])\s+/))) {
        // A list: its items, each with the lines indented under it (a nested list among them).
        const ordered = /\d/.test(m[2]), indent = m[1].length, items = [];
        while (i < lines.length) {
          const line = lines[i], ind = line.match(/^\s*/)[0].length;
          const im = line.match(/^(\s*)([-*+]|\d+[.)])\s+(.*)$/);
          if (im && ind === indent && /\d/.test(im[2]) === ordered) { items.push([im[3]]); i++; continue; }
          if (!items.length) break;
          if (line.trim() && ind > indent) { items[items.length - 1].push(line.slice(Math.min(ind, indent + 2))); i++; continue; }
          // A line of the item's text that wraps without its indent.
          if (line.trim() && !im && lines[i - 1]?.trim() && !/^\s*(#{1,6}\s|```|~~~|>|\|)/.test(line)) { items[items.length - 1].push(line.trim()); i++; continue; }
          // A blank line goes on with the list when what follows is indented under it, or its next item.
          const next = lines[i + 1] || "", ni = next.match(/^\s*/)[0].length;
          if (!line.trim() && next.trim() && (ni > indent || (ni === indent && /^\s*([-*+]|\d+[.)])\s/.test(next)))) { i++; continue; }
          break;
        }
        const item = (ls) => {
          const task = ls[0].match(/^\[([ xX])\]\s+/);
          if (task) ls = [ls[0].slice(task[0].length), ...ls.slice(1)];
          const box = task ? `<input type="checkbox" disabled${task[1] !== " " ? " checked" : ""}> ` : "";
          return `<li>${box}${ls.length === 1 ? inline(ls[0]) : block(ls)}</li>`;
        };
        out.push(`<${ordered ? "ol" : "ul"}>${items.map(item).join("")}</${ordered ? "ol" : "ul"}>`);
      } else {
        const p = [];
        for (; i < lines.length && lines[i].trim() && !/^\s*(#{1,6}\s|```|~~~|>|([-*+]|\d+[.)])\s)/.test(lines[i]); i++) p.push(lines[i].trim());
        if (!p.length) { p.push(l.trim()); i++; }
        out.push(`<p>${p.map(inline).join(" ")}</p>`);
      }
    }
    return out.join("");
  };
  return block(String(src).replace(/\r\n?/g, "\n").split("\n"));
}

// Where a README's relative link goes: the file or folder of the tree at this position, an image
// of the tree drawn from its object.
function readmeLinks(repo, dir, tree, at, head) {
  return (url) => {
    const [path, hash] = url.split("#");
    const parts = (path.startsWith("/") ? [] : dir ? dir.split("/") : []);
    for (const p of path.split("/")) { if (p === "..") parts.pop(); else if (p && p !== ".") parts.push(p); }
    const full = parts.join("/");
    const e = tree.files[full];
    const href = (full ? codeHref(repo, full, at, head) : `${P(repo)}/code${atQ(at, head)}`) + (hash ? `#${hash}` : "");
    return e && /\.(png|jpe?g|gif|svg|webp)$/i.test(full) ? { href, img: `${R(repo)}/objects/${e.blob}` } : { href };
  };
}

// ---- Code: the tree, a file, blame ----

async function code(el, [repo, rest], q, n) {
  const path = (rest || "").replace(/^\/+|\/+$/g, "");
  const tabs = await repoFrame(el, repo, "code");
  const head = await headOf(repo);
  if (!current(n)) return;
  if (!head) {
    el.innerHTML = tabs + repoTitle(repo) + empty("Nothing on the line yet", "Put the project's files on it from its folder:", setupCommands(repo), { icon: "folder" });
    return;
  }
  const at = q.has("at") ? Number(q.get("at")) : head.position;
  const tree = await treeAt(repo, at);
  if (!current(n)) return;
  const pos = at === head.position ? head : (await api(`${R(repo)}/positions?from=${at}&limit=1`))[0];
  if (!current(n)) return;
  const atBar = `<div class="bar"><div class="grow">${pathCrumbs(repo, path, at, head.position)}</div>
    <span class="small soft">position <a href="${P(repo)}/p/${at}">${at}</a>${at !== head.position ? ` · <a href="${P(repo)}/code/${path}">see the head (${head.position})</a>` : ""}</span></div>`;
  const change = ["edit", "new", "upload", "delete"].find((k) => q.has(k));
  if (change && at === head.position && mayWrite(repo)) return changePage(el, tabs, repo, path, tree, head, change, n);
  if (path && tree.files[path]) return fileView(el, tabs, atBar, repo, path, tree.files[path], at, head.position, q.get("view") === "blame", n);
  const d = tree.dirs.get(path);
  if (!d) {
    el.innerHTML = tabs + atBar + failed(new ApiError(404, `No ${path} at position ${at}.`), false);
    return;
  }
  const rows = [
    ...[...d.dirs].sort().map((name) => `<a class="item" href="${codeHref(repo, path ? `${path}/${name}` : name, at, head.position)}"><span class="main"><span class="title">${icon("folder")}${esc(name)}</span></span></a>`),
    ...d.files.sort().map((name) => `<a class="item" href="${codeHref(repo, path ? `${path}/${name}` : name, at, head.position)}"><span class="main"><span class="title">${icon("file")}<span class="mono">${esc(name)}</span></span></span></a>`),
  ];
  const readme = d.files.find((f) => /^readme(\.md|\.markdown|\.txt)?$/i.test(f));
  const adds = at === head.position && mayWrite(repo) ? `<a class="button sm" href="${changeHref(repo, path, "new")}">${icon("plus")}Add file</a><a class="button sm" href="${changeHref(repo, path, "upload")}">${icon("up")}Upload</a>` : "";
  const listing = tree.count
    ? `<div class="list tree">${lastLanding(repo, pos, head)}${rows.join("") || `<div class="item soft">Empty directory</div>`}</div>`
    : `<div class="list tree">${lastLanding(repo, pos, head)}</div>` + empty("No file yet", "An agent opens a task, writes the first files and lands them:", [`avp open --repo ${repo} --goal "First files"`], { icon: "folder" });
  el.innerHTML = tabs + (path ? "" : repoTitle(repo, head)) + `<div class="split"><section>${atBar}${adds ? `<div class="bar adds">${adds}</div>` : ""}${listing}
      <div id="readme">${readme ? loading("README") : ""}</div></section>
    <aside class="stack">${repoAside(repo, head)}</aside></div>`;
  if (readme) {
    const file = path ? `${path}/${readme}` : readme;
    fetch(`${R(repo)}/objects/${tree.files[file].blob}`, { headers: LIVE }).then((r) => r.ok ? r.text() : "").then((t) => {
      const box = $("readme");
      if (!current(n) || !box) return;
      const body = /\.(md|markdown)$/i.test(readme) ? `<div class="md">${markdown(t.slice(0, 200000), readmeLinks(repo, path, tree, at, head.position))}</div>` : `<pre class="md-plain">${esc(t.slice(0, 200000))}</pre>`;
      box.innerHTML = t ? `<div class="file readme"><div class="fhead"><span class="grow mono">${esc(readme)}</span><a class="button sm" href="${codeHref(repo, file, at, head.position)}">Source</a></div>${body}</div>` : "";
    }).catch(() => { const box = $("readme"); if (box) box.innerHTML = ""; });
  }
}

// The repo's name over its home, as GitHub puts it over the code, with the way to connect an agent.
function repoTitle(repo, head) {
  const about = ABOUT_NOW.get(repo) || {};
  return `<div class="repohead"><div class="grow"><h1>${repoLabel(about.name || repo)} <span class="pill">${about.visibility === "public" ? "Public" : "Private"}</span></h1>${about.description ? `<p class="soft">${esc(about.description)}</p>` : ""}</div>
    ${head ? `<a class="button" href="${P(repo)}/activity">${icon("activity")}Activity</a>` : ""}</div>`;
}

// The last landing over the tree: its goal, who landed it and through which agent, its proof, and
// how many positions the line holds.
function lastLanding(repo, pos, head) {
  if (!pos) return "";
  const who = pos.actor?.person || (pos.position === 0 ? (madeBy(pos).match(/ by (.+)$/)?.[1] || "AVP") : "someone");
  return `<div class="item head last">${avatar(who)}
    <span class="main"><span class="title"><strong>${esc(who)}</strong> <a href="${P(repo)}/p/${pos.position}">${esc(short(noteTitle(pos.note), 90))}</a></span>
      <span class="meta">${pos.actor?.agent && pos.actor.agent !== "human" ? `through ${esc(agentName(pos.actor.agent))} · ` : ""}${pos.proof ? `proof <a href="${P(repo)}/proofs/${enc(pos.proof)}">${noChecks(pos.verdict, pos.steps) ? "no checks" : esc(pos.verdict || "run")}</a>${pos.seconds != null ? ` in ${dur(pos.seconds)}` : ""} · ` : "no proof · "}${ago(pos.landed_at)}</span></span>
    <span class="side">${pos.proof ? proofPill(pos.verdict, pos.steps) : ""} ${chip(pos.position, repo)} <a href="${P(repo)}/line" class="hide-sm" style="margin-left:var(--avp-space-2)">${icon("history")} ${plural(head.position + 1, "position")}</a></span></div>`;
}

function repoAside(repo, head) {
  return `<div class="card clone"><span class="label">Clone</span>
      <p class="small soft">HTTPS, for Git: sign in with any name and a token from Account as the password</p>${cmd(`git clone ${cloneUrl(repo)}`)}
      <p class="small soft">For an agent: a task on the head, laid out in its folder</p>${cmd(`avp open --repo ${repo} --goal "..."`)}</div>
    <div class="card"><span class="label">The line</span><p class="big">${(head.position + 1).toLocaleString(LOCALE)}<span class="soft"> ${head.position ? "positions" : "position"}</span></p>
      <p class="small soft">Each one a task landed after its checks.</p><p class="small"><a href="${P(repo)}/line">History</a> · <a href="${P(repo)}/proofs">Proofs</a> · <a href="${P(repo)}/activity">Activity</a></p></div>
    <div class="card"><span class="label">Connect your agents</span><p class="small soft">Connect yours once: it opens tasks on this line and lands them, proven, without the web.</p>${agentTiles(repo)}<a class="button wide" href="${connectHref(repo)}">${icon("plug")}Connect an agent</a></div>`;
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
  const fhead = `<div class="fhead"><span class="grow mono">${esc(path.split("/").pop())}</span><span class="soft">${size(buf.length)}</span>${binary ? "" : toggle}<a class="button" href="${P(repo)}/history/${path.split("/").map(enc).join("/")}">History</a><a class="button" href="${raw}?name=${enc(path.split("/").pop())}"${binary ? ` download="${esc(path.split("/").pop())}"` : ""}>Raw</a>${at === head && mayWrite(repo) ? `${binary ? "" : `<a class="button" href="${changeHref(repo, path, "edit")}">Edit</a>`}<a class="button danger" href="${changeHref(repo, path, "delete")}">Delete</a>` : ""}</div>`;
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
    (log.changes.length ? `<div class="list">${log.changes.map((c) => `<a class="item" href="${P(repo)}/p/${c.position}"><span class="pos">${c.position}</span><span class="main"><span class="title">${esc(short(c.goal, 110))}</span><span class="meta">${[c.change, actorLine(c), ago(c.landed_at)].filter(Boolean).map(esc).join(" · ")}</span></span></a>`).join("")}</div>` : empty("No change found", "No position touched this path."))
    + (log.more != null ? `<p><a class="button" href="?from=${log.more}">Older</a></p>` : "");
});

// ---- the line ----

function positionItem(repo, p) {
  return `<a class="item" href="${P(repo)}/p/${p.position}"><span class="pos">${p.position}</span>
    <span class="main"><span class="title">${esc(short(noteTitle(p.note), 120))}</span><span class="meta">${esc(actorLine(p.actor) || madeBy(p))} · ${ago(p.landed_at)}${p.tags?.length ? ` · ${p.tags.map(esc).join(", ")}` : ""}</span></span>
    <span class="side">${p.proof ? `${proofPill(p.verdict, p.steps)}<br><span class="hide-sm">${dur(p.seconds)}</span>` : ""}</span></a>`;
}

page(repoPage(String.raw`\/line`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "line");
  const from = q.get("from");
  const ps = await api(`${R(repo)}/positions?limit=50${from ? `&from=${from}` : ""}`);
  if (!current(n)) return;
  if (!ps.length) {
    el.innerHTML = tabs + empty("Nothing landed yet", "Each landed task becomes a position here.", setupCommands(repo));
    return;
  }
  const last = ps[ps.length - 1].position;
  const length = await lineLength(repo);
  if (!current(n)) return;
  el.innerHTML = tabs + `<div class="pagehead"><div class="grow"><h1>History</h1><p class="soft">${plural(length, "position")}, newest first: each one a landed change.</p></div><a class="button" href="${P(repo)}/activity">${icon("activity")}Watch it live</a></div>
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
    <div class="bar"><h1 class="grow" style="margin:0">${esc(short(noteTitle(p.note), 200))}</h1>${p.position > 0 ? `<a class="button" href="${P(repo)}/p/${p.position - 1}">Previous</a>` : ""}${head && p.position < head.position ? `<a class="button" href="${P(repo)}/p/${p.position + 1}">Next</a>` : ""}</div>
    <div class="split"><section>${p.position === 0 ? empty("Where the line starts", `The files the repo began with. <a href="${P(repo)}/code?at=0">Browse them</a>.`) : diffView(diff)}</section>
    <aside class="stack"><div class="card"><dl class="facts">
      <dt>Position</dt><dd>${chip(p.position)}</dd>
      ${imported(p.note) ? `<dt>Author</dt><dd>${esc(imported(p.note).author)}</dd><dt>From</dt><dd>Git, ${esc(imported(p.note).date)}</dd>` : `<dt>Person</dt><dd>${esc(p.actor?.person || madeBy(p).match(/ by (.+)$/)?.[1] || "nobody")}</dd>
      <dt>Agent</dt><dd>${esc(p.actor ? agentName(p.actor.agent) : madeBy(p).startsWith("created on the web") ? "the website" : "none")}</dd>`}
      <dt>Landed</dt><dd>${when(p.landed_at)}${took ? `<br><span class="small soft">${took} after it opened</span>` : ""}</dd>
      <dt>Proof</dt><dd>${p.proof ? `<a href="${P(repo)}/proofs/${enc(p.proof)}">${noChecks(p.verdict, p.steps) ? "no checks" : esc(p.verdict || "proof")}</a> · ${dur(p.seconds)}` : `<span class="soft">${imported(p.note) ? "imported, not proven here" : "none"}</span>`}</dd>
      ${p.full ? `<dt>Full suite</dt><dd><a href="${P(repo)}/proofs/${enc(p.full)}">its proof</a></dd>` : ""}
      ${p.base_position != null && !noChecks(p.verdict, p.steps) ? `<dt>Proven on</dt><dd>position ${p.base_position}</dd>` : ""}
      ${p.task ? `<dt>Task</dt><dd>${idChip(p.task)}</dd>` : ""}
      ${p.git ? `<dt>Git</dt><dd>${idChip(p.git.slice(0, 12))}</dd>` : ""}
      ${p.tags?.length ? `<dt>Tags</dt><dd>${p.tags.map((t) => `<span class="pill">${esc(t)}</span>`).join(" ")}</dd>` : ""}
    </dl></div>
    <div class="card"><h3>Browse</h3><p><a href="${P(repo)}/code?at=${p.position}">The code at this position</a></p>${cmd(`avp show ${p.position}`)}</div></aside></div>`;
});

// ---- tasks ----

page(repoPage(String.raw`\/tasks`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "tasks");
  const ts = await api(`${R(repo)}/tasks?state=open`);
  if (!current(n)) return;
  el.innerHTML = tabs + `<h2>Open tasks</h2>${ts.length ? `<p class="soft small">Each one waits for its agent or person to land it, or to abandon it. A change made on the web that did not land is here: open it to land it again or abandon it.</p>` : ""}` + (ts.length
    ? `<div class="list">${ts.sort((a, b) => b.created_at - a.created_at).map((t) => `<a class="item" href="${P(repo)}/tasks/${enc(t.id)}"><span class="main"><span class="title">${esc(short(t.goal, 120))}</span><span class="meta">${esc(actorLine(t.actor) || "a person")}${t.actor?.agent ? "" : " on the web"} · open since ${when(t.created_at)} (${ago(t.created_at)}) on position ${t.base_position}</span></span><span class="side hide-sm">${t.scope.slice(0, 3).map(esc).join(", ")}${t.scope.length > 3 ? "…" : ""}</span></a>`).join("")}</div>`
    : empty("No open task", "An agent opens one, writes, lands it: the line moves by tasks.", [`avp open --repo ${repo} --goal "what this change does"`]));
});

page(repoPage(String.raw`\/tasks\/([^/]+)`), async (el, [repo, id], q, n) => {
  const tabs = await repoFrame(el, repo, "tasks", [["task"]]);
  const [st, diff, evs] = await Promise.all([api(`${R(repo)}/tasks/${enc(id)}`), api(`${R(repo)}/tasks/${enc(id)}/diff`).catch(() => []), api(`${R(repo)}/events?task=${enc(id)}&limit=1000`).catch(() => [])]);
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
      <dt>Id</dt><dd>${idChip(t.id)}</dd></dl></div>
      <div class="card"><h3>Wait for it</h3><p class="small soft">An agent that needs this work waits for it to land:</p>${cmd(`avp wait --task ${t.id}`)}</div></aside></div>`;
  if (t.state !== "open" || !mayWrite(repo)) return;
  // Waiting for a person's word: what it changes in the guarded paths, then the word itself. Only a
  // person signed in here gives it: the server refuses it from a token, an agent's.
  const asked = [...evs].reverse().find((e) => e.kind === "task.awaiting_approval");
  if (asked && !evs.some((e) => e.id > asked.id && /^task\.(approved|refused)$/.test(e.kind))) {
    const guarded = asked.data?.guarded || [];
    const shown = guarded.length ? diff.filter((f) => guarded.includes(f.path)) : [];
    el.querySelector(".split section").insertAdjacentHTML("afterbegin", `<div class="card landing" id="word"><div class="bar" style="margin:0"><span class="dot wait"></span><strong class="grow">Waiting for a person's approval</strong></div>
      <p style="margin-top:var(--avp-space-3)">${guarded.length ? `It changes ${guarded.map((g) => `<span class="mono">${esc(g)}</span>`).join(", ")}, guarded: it lands once a person approves it, proven as written.` : "This repo lands a change once a person approves it, proven as written."}</p>
      ${shown.length ? diffView(shown) : ""}<div class="bar" style="margin:0"><span class="grow"></span><button id="reject-task">Reject</button><button class="primary" id="approve-task">Approve</button></div></div>`);
    const word = async (ev, approved) => {
      const reason = approved ? "" : prompt("Why? The agent reads it with its next land.");
      if (reason === null) return;
      ev.target.disabled = true;
      try {
        const a = await api(`${R(repo)}/tasks/${enc(t.id)}/approve`, { method: "POST", body: { person: ME?.person || "", approved, reason } });
        toast(approved ? `Approved by ${a.person}: it lands now` : `Rejected by ${a.person}`);
        $("word").remove();
      } catch (e) { ev.target.disabled = false; toast(e.message); }
    };
    $("approve-task").addEventListener("click", (ev) => word(ev, true));
    $("reject-task").addEventListener("click", (ev) => word(ev, false));
  }
  // Open, it is landed again or dropped from here: a change made on the web has no folder to do it from.
  el.querySelector(".bar").insertAdjacentHTML("beforeend", `<button id="abandon-task">Abandon</button><button class="primary" id="land-again">Land again</button>`);
  el.querySelector(".split section").insertAdjacentHTML("afterbegin", `<div id="landing"></div>`);
  const ctx = { repo, head: { position: st.head_position }, back: `${P(repo)}/tasks`, done: P(repo), state: { task: t.id } };
  $("land-again").addEventListener("click", async (ev) => {
    ev.target.disabled = true;
    const box = $("landing");
    box.innerHTML = `<div class="card landing"><div class="bar" style="margin:0"><span class="dot run"></span><strong class="grow">Proving on the head of the line</strong></div></div>`;
    const stop = landing();
    try {
      const { status, body } = await landRequest(repo, t.id);
      box.innerHTML = await landed(ctx, t.id, status === 200 ? body : null, status === 409 ? body : null, null);
    } catch (e) {
      box.innerHTML = `<div class="card landing red"><strong>Not landed</strong><p>${esc(e.message)}</p></div>`;
    }
    stop();
    ev.target.disabled = false;
    $("drop-task")?.addEventListener("click", abandon);
  });
  const abandon = async (ev) => {
    if (!confirm("Abandon this task? Its change is dropped, the line does not move.")) return;
    ev.target.disabled = true;
    try {
      await api(`${R(repo)}/tasks/${enc(t.id)}/abandon`, { method: "POST" });
      OPEN.delete(repo);
      go(`${P(repo)}/tasks`);
    } catch (e) { ev.target.disabled = false; toast(e.message); }
  };
  $("abandon-task").addEventListener("click", abandon);
});

// ---- proofs ----

page(repoPage(String.raw`\/proofs`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "proofs");
  const from = q.get("from");
  const ps = (await api(`${R(repo)}/positions?limit=100${from ? `&from=${from}` : ""}`));
  if (!current(n)) return;
  const proven = ps.filter((p) => p.proof);
  const slowest = Math.max(1, ...proven.map((p) => p.seconds || 0));
  el.innerHTML = tabs + `<h2>Proofs of the line</h2><p class="soft small">Each landing is checked on the latest version first: these are the checks that let it land.</p>` + (proven.length
    ? `<div class="list steps">${proven.map((p) => `<a class="item" href="${P(repo)}/proofs/${enc(p.proof)}"><span class="pos">${p.position}</span><span class="main"><span class="title">${esc(short(p.note, 100))}</span><span class="meta">${p.steps.length ? `${plural(p.steps.length, "step")} · ${p.steps.filter((s) => s.exit !== 0).length ? "a red step" : "every step green"}` : "no checks"} · ${ago(p.landed_at)}</span></span>
      <span class="bar-t hide-sm"><i style="width:${(100 * (p.seconds || 0) / slowest).toFixed(1)}%"></i></span><span class="side">${proofPill(p.verdict, p.steps)}<br>${dur(p.seconds)}</span></a>`).join("")}</div>`
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
  const red = p.steps.filter((s) => s.exit !== 0).length;
  el.innerHTML = tabs + `<div class="pagehead"><div class="grow"><span class="label">Proof</span><h1>${!p.steps.length ? "No checks ran" : p.verdict === "green" ? "Every step green" : red ? `${plural(red, "step")} red` : esc(p.verdict)}</h1>
      <p class="soft">${p.steps.length ? `${plural(p.steps.length, "step")} in ${dur(total)}` : "Add checks to avp.toml and every landing runs them"} · proven on position ${p.base_position} · ${when(p.created_at)}</p></div>${proofPill(p.verdict, p.steps)}</div>
    <div class="split"><section><div class="list steps">${p.steps.map((s, i) => `<div class="item" style="flex-wrap:wrap"><span class="dot ${s.exit === 0 ? "green" : "red"}"></span>
      <span class="main"><span class="title">${esc(s.name)}</span><span class="meta mono">${esc(s.command || "")}</span></span>
      <span class="bar-t"><i style="width:${(100 * (s.seconds || 0) / longest).toFixed(1)}%"></i></span><span class="side">${s.exit === 0 ? "" : `exit ${s.exit}<br>`}${dur(s.seconds)}</span>
      <button class="sm" data-step="${i}">Log</button><div class="log-box" style="flex-basis:100%"></div></div>`).join("")}</div></section>
    <aside class="stack"><div class="card"><dl class="facts">
      <dt>Verdict</dt><dd>${p.steps.length ? esc(p.verdict) : "no checks"}</dd>
      <dt>Steps</dt><dd>${dur(total)}</dd>
      ${t.total_s != null ? `<dt>End to end</dt><dd>${dur(t.total_s)}</dd><dt>Waiting</dt><dd>${dur(t.wait_s)}</dd><dt>Preparing</dt><dd>${dur(t.prep_s)}</dd>${t.setup_ran ? `<dt>Setup</dt><dd>${dur(t.setup_s)}</dd>` : ""}` : ""}
      <dt>Proven on</dt><dd>position ${p.base_position}</dd>
      <dt>Files</dt><dd class="small">${(p.files || []).slice(0, 12).map((f) => `<span class="mono">${esc(f)}</span>`).join("<br>")}${(p.files || []).length > 12 ? `<br>and ${p.files.length - 12} more` : ""}</dd>
      ${p.blamed?.length ? `<dt>Blamed</dt><dd class="small">${p.blamed.map(esc).join("<br>")}</dd>` : ""}
      <dt>Id</dt><dd>${idChip(p.id)}</dd></dl></div>${cmd(`avp proof ${p.id}`)}</aside></div>`;
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

// ---- activity: the line as it moves (what AVP Live showed, as a section of the repo) ----
// Open tasks with their last proof, the proofs waiting and running, the environments and their
// deploys, then the feed of events. Read again every few seconds while the page is shown; each row
// opens its detail in place, and stays open across the reads.

const TICK_MS = 4000;

page(repoPage(String.raw`\/activity`), async (el, [repo], q, n) => {
  const tabs = await repoFrame(el, repo, "activity", [["Activity"]]);
  const A = { evs: [], ids: new Set(), last: 0, goals: new Map(), tasks: [], statuses: new Map(), envs: [], conf: {}, work: null, head: null,
    ok: true, at: 0, open: new Set(), proofs: new Map(), files: new Map(), logs: new Map(), fresh: new Set() };
  const absorb = (evs) => {
    const fresh = evs.filter((e) => !A.ids.has(e.id));
    fresh.forEach((e) => A.ids.add(e.id));
    A.evs.push(...fresh);
    A.evs.sort((a, b) => a.id - b.id);
    if (A.evs.length > 600) A.evs.splice(0, A.evs.length - 600).forEach((e) => A.ids.delete(e.id));
    goalsOf(fresh, A.goals);
    if (fresh.length) A.last = Math.max(A.last, ...fresh.map((e) => e.id));
    return fresh;
  };
  const [first, envFirst, cfg] = await Promise.all([
    api(`${R(repo)}/events?last=300`).catch(() => []),
    api(`${R(repo)}/events?kinds=env.,deploy.&last=100`).catch(() => []),
    api(`${R(repo)}/config`).catch(() => ({ config: {} })),
  ]);
  if (!current(n)) return;
  absorb([...first, ...envFirst]);
  A.conf = cfg.config?.envs || {};
  field(true);
  el.innerHTML = tabs + `<div id="act"><div class="pagehead"><div class="grow"><h1>Activity</h1><p class="soft">The line as it moves: tasks open and land, proofs run on the head, environments deploy. Every row opens in place.</p></div>
      <span class="live-dot" id="act-live">Live</span></div>
    <div class="tiles" id="act-tiles"></div>
    <div class="section-h"><h2>Open tasks</h2><span class="count" id="act-tcount">0</span><span class="grow"></span><a class="button ghost sm" href="${P(repo)}/tasks">All tasks</a></div><div id="act-tasks">${loading()}</div>
    <div class="section-h"><h2>Environments</h2><span class="grow"></span><a class="button ghost sm" href="${P(repo)}/envs">Details</a></div><div id="act-envs">${loading()}</div>
    <div class="section-h"><h2>Feed</h2><span class="grow"></span><span class="soft small" id="act-at"></span></div><div id="act-feed"></div></div>`;
  // Listened on this page's own box: the next page draws a new one, and these go with it.
  const box = $("act");
  const put = (id, html) => { const box = $(id); if (box && box.dataset.h !== html) { box.innerHTML = html; box.dataset.h = html; } };

  // A proof's steps, each with its log a click away (read once, kept while the page lives).
  const stepsOf = (pf) => `<div class="list steps">${pf.steps.map((s) => {
    const k = `${pf.id}\n${s.name}`, lg = A.logs.get(k);
    return `<div class="item${s.exit !== 0 ? " red" : ""}" style="flex-wrap:wrap">${verdictDot(s.exit === 0 ? "green" : "red")}<span class="main"><span class="title">${esc(s.name)}</span><span class="meta mono">${esc(s.command || "")}</span></span>
      <span class="side">${s.exit === 0 ? "" : `exit ${esc(s.exit)} · `}${dur(s.seconds)}</span><button type="button" class="ghost sm" data-log="${esc(k)}">${lg === undefined ? "Log" : "Hide"}</button>
      ${lg === undefined ? "" : `<pre class="log" style="flex-basis:100%;margin-top:var(--avp-space-2)">${lg === null ? "Reading the log" : esc(lg)}</pre>`}</div>`;
  }).join("")}</div>`;
  const proofOf = (id) => {
    if (!id) return "";
    if (!A.proofs.has(id)) { A.proofs.set(id, null); api(`${R(repo)}/proofs/${enc(id)}`).then((x) => A.proofs.set(id, x), () => A.proofs.set(id, false)).then(draw); }
    const pf = A.proofs.get(id);
    return pf === null ? loading("Proof") : pf === false ? `<p class="soft small">Its proof could not be read.</p>` : stepsOf(pf);
  };
  const filesOf = (pos) => {
    if (!pos) return "";
    if (!A.files.has(pos)) { A.files.set(pos, null); api(`${R(repo)}/line/${pos}/diff`).then((x) => A.files.set(pos, x), () => A.files.set(pos, false)).then(draw); }
    const f = A.files.get(pos);
    if (!f) return f === null ? loading("Files") : "";
    return `<p class="label" style="margin:var(--avp-space-4) 0 var(--avp-space-2)">${plural(f.length, "file")} changed</p><div class="list">${f.slice(0, 12).map((x) => `<a class="item" href="${codeHref(repo, x.path, null, null)}">${icon("file")}<span class="main"><span class="title mono">${esc(x.path)}</span></span><span class="pill">${esc(x.change)}</span></a>`).join("")}</div>${f.length > 12 ? `<p class="soft small">and ${f.length - 12} more</p>` : ""}`;
  };
  const row = (k, mark, title, meta, end, detail, fresh = false) => `<details data-k="${esc(k)}"${fresh ? ` class="fresh"` : ""}${A.open.has(k) ? " open" : ""}><summary><span class="glyph">${mark}</span><span class="what"><span class="title">${title}</span><span class="meta">${meta}</span></span><span class="end">${end}${icon("right", "chev")}</span></summary>${A.open.has(k) ? `<div class="detail">${detail()}</div>` : ""}</details>`;

  function draw() {
    if (!current(n)) return;
    const head = A.head;
    const wk = A.work;
    const tile = (label, big, sub = "") => `<div class="tile"><span class="label">${esc(label)}</span><p class="big">${big}${sub ? ` <span class="soft">${sub}</span>` : ""}</p></div>`;
    put("act-tiles", tile("Head", head ? `#${head.position}` : "none", head ? esc(ago(head.landed_at)) : "")
      + tile("Open tasks", String(A.tasks.length))
      + tile("Proofs", wk ? String(wk.running) : "0", wk ? `running · ${wk.waiting} waiting` : "running")
      + tile("Last proof", head?.verdict ? esc(head.verdict) : "none", head?.seconds != null ? esc(dur(head.seconds)) : ""));
    $("act-tcount").textContent = A.tasks.length;
    $("act-live")?.classList.toggle("off", !A.ok);
    $("act-at").textContent = A.at ? `read ${clock(A.at)}` : "";
    const ts = [...A.tasks].sort((a, b) => b.created_at - a.created_at);
    put("act-tasks", ts.length ? `<div class="rows">${ts.map((t) => {
      const st = A.statuses.get(t.id), pf = st?.last_proof;
      const writes = st ? st.writes.filter((w) => !w.undone).length : 0;
      const files = [...new Set((st?.leases || []).map((l) => l.path))];
      const waits = A.evs.some((e) => e.task === t.id && e.kind === "task.awaiting_approval") && !A.evs.some((e) => e.task === t.id && /task\.(approved|refused)/.test(e.kind));
      const mark = pf ? verdictDot(pf.verdict) : avatar(t.actor?.person || "?");
      const end = `${waits ? `<span class="pill run">approval</span>` : pf ? verdictPill(pf.verdict) : `<span class="pill">not proven</span>`} <span class="hide-sm">${timeAgo(t.created_at)}</span>`;
      return row(`t${t.id}`, mark, esc(short(t.goal, 120)), `${esc(actorLine(t.actor) || "a person on the web")} · on ${chipText(t.base_position)} · ${plural(writes, "write")}${files.length ? ` · ${plural(files.length, "path")} held` : ""}`, end, () => `
        ${files.length ? `<p class="label" style="margin-bottom:var(--avp-space-2)">Holds</p><p>${files.slice(0, 8).map((f) => `<span class="idchip" style="margin:0 4px 4px 0">${esc(f)}</span>`).join("")}${files.length > 8 ? ` and ${files.length - 8} more` : ""}</p>` : ""}
        ${pf ? `<p class="label" style="margin:var(--avp-space-3) 0 var(--avp-space-2)">Last proof, on ${chipText(pf.base_position)}</p>${stepsOf(pf)}` : `<p class="soft small">Not proven yet: <code>avp land</code> proves it on the head.</p>`}
        <div class="bar"><a class="button sm" href="${P(repo)}/tasks/${enc(t.id)}">Open the task</a><span class="grow"></span>${idChip(t.id)}</div>`);
    }).join("")}</div>` : empty("No task open", "An agent opens one, writes, lands it: it shows here as it goes.", [], { icon: "tasks", boxed: true }));
    const envs = A.envs;
    put("act-envs", envs.length ? `<div class="envs">${envs.map((e) => {
      const c = A.conf[e.name] || {}, w = e.wanted, pr = e.progress;
      const moving = w && w.position !== e.position;
      const failed = pr && /fail|red|error/i.test(pr.status || pr.state || "");
      return `<div class="env"><div class="top-row">${verdictDot(failed ? "red" : moving ? "running" : e.position >= 0 ? "green" : "")}<strong>${esc(e.name)}</strong>${e.position >= 0 ? chip(e.position, repo) : ""}</div>
        <div class="meta">${e.position >= 0 ? `serves since ${esc(ago(e.moved_at))}` : "nothing known yet"}${c.after ? ` · after ${esc(c.after)}` : ""}</div>
        ${moving ? `<div class="meta" style="margin-top:var(--avp-space-1)"><span class="pill run">deploying #${esc(w.position)}</span>${w.by ? ` by ${esc(w.by)}` : ""}</div>` : ""}
        ${pr ? `<div class="meta mono" style="margin-top:var(--avp-space-1)">${esc(pr.status || pr.state || "")} ${esc(pr.line || pr.message || "")}</div>` : ""}</div>`;
    }).join("")}</div>` : empty("No environment", "Say where the line deploys in avp.toml: promotions and deploys show here.", [], { icon: "envs", boxed: true }));
    const feed = [];
    for (let i = A.evs.length - 1; i >= 0 && feed.length < 40; i--) { const e = A.evs[i], ph = eventPhrase(e, A.goals.get(e.task)); if (ph) feed.push([e, ph]); }
    put("act-feed", feed.length ? `<div class="rows">${feed.map(([e, ph]) => {
      const who = e.kind === "task.opened" ? "" : A.evs.find((x) => x.task === e.task && x.kind === "task.opened")?.data?.actor;
      const meta = [e.kind.replace(/^task\.|^line\./, "").replace(/_/g, " "), who ? actorLine(who) : ""].filter(Boolean).map(esc).join(" · ");
      const end = `${ph.pos != null ? chip(ph.pos, repo) : ""} ${timeAgo(e.at)}`;
      return row(`e${e.id}`, eventMark(ph), ph.text, meta, end, () => `
        ${ph.proof ? `<p class="label" style="margin-bottom:var(--avp-space-2)">Proof</p>${proofOf(ph.proof)}` : ""}
        ${ph.pos != null && e.kind === "task.landed" ? filesOf(ph.pos) : ""}
        ${e.data?.reason ? `<p>${esc(e.data.reason)}</p>` : ""}${e.data?.url || e.data?.run ? `<p><a href="${esc(e.data.url || e.data.run)}" target="_blank" rel="noopener">The deploy's run</a></p>` : ""}
        <div class="bar">${ph.pos != null ? `<a class="button sm" href="${P(repo)}/p/${ph.pos}">Open position #${esc(ph.pos)}</a>` : ""}${e.task ? `<a class="button sm ghost" href="${P(repo)}/tasks/${enc(e.task)}">The task</a>` : ""}<span class="grow"></span><span class="soft small">${esc(when(e.at))}</span></div>`, A.fresh.has(e.id));
    }).join("")}</div>` : empty("Nothing moved yet", "The first task an agent opens shows up here, live.", [`avp open --repo ${repo} --goal "what this change does"`], { icon: "activity", boxed: true }));
    A.fresh.clear();
  }

  box.addEventListener("toggle", (ev) => {
    const d = ev.target;
    if (!d.matches?.("details[data-k]") || A.open.has(d.dataset.k) === d.open) return;
    if (d.open) A.open.add(d.dataset.k); else A.open.delete(d.dataset.k);
    draw();
  }, true);
  box.addEventListener("click", (ev) => {
    const b = ev.target.closest("[data-log]");
    if (!b) return;
    ev.preventDefault();
    const k = b.dataset.log, [pid, step] = k.split("\n");
    if (A.logs.has(k)) { A.logs.delete(k); return draw(); }
    A.logs.set(k, null);
    draw();
    api(`${R(repo)}/proofs/${enc(pid)}/log?step=${enc(step)}&window=400`).then((l) => (l.lines || []).map((x) => (Array.isArray(x) ? x[1] : x.text ?? x)).join("\n") || "(no output)", () => "(no log kept)")
      .then((t) => { A.logs.set(k, t); draw(); });
  });

  // The loop: what changed since the last read, the open tasks and their status, the environments.
  while (current(n)) {
    if (!document.hidden) {
      try {
        const [evs, tasks, envs, work, head] = await Promise.all([api(`${R(repo)}/events?since=${A.last}&limit=500`), api(`${R(repo)}/tasks?state=open`), api(`${R(repo)}/envs`).catch(() => []), maybe("/work").catch(() => null), headOf(repo).catch(() => A.head)]);
        if (!current(n)) return;
        const fresh = absorb(evs);
        // Events of this read slide into the feed once: the next draw shows them as rows like any other.
        A.fresh = new Set(fresh.map((e) => e.id));
        A.tasks = tasks.filter((t) => /^open$/i.test(t.state));
        A.envs = envs;
        A.work = work ? { running: 0, waiting: 0, ...work } : null;
        A.head = head;
        const dirty = new Set(fresh.map((e) => e.task).filter(Boolean));
        await Promise.all(A.tasks.filter((t) => !A.statuses.has(t.id) || dirty.has(t.id)).map((t) => api(`${R(repo)}/tasks/${enc(t.id)}`).then((st) => A.statuses.set(t.id, st), () => {})));
        if (fresh.some((e) => e.kind === "task.landed")) { OPEN.delete(repo); LENGTH.delete(repo); }
        A.ok = true;
        A.at = Date.now();
      } catch { A.ok = false; }
      if (!current(n)) return;
      draw();
    }
    await new Promise((ok) => setTimeout(ok, TICK_MS));
  }
});

// ---- settings: who reaches it, its tokens, its name and visibility, what guards it, its log, the
// danger zone. Every control is a route the CLI (`avp repo`) and the avp_repo tool send too; the
// server checks the role, the page only hides what would be refused. ----

const ROLE_DOES = { read: "reads the line and clones it", write: "opens and lands tasks, approves guarded changes", admin: "also changes these settings" };
const roleSelect = (name, on, roles = ["read", "write", "admin"]) => `<select name="${name}" style="width:110px">${roles.map((r) => `<option${r === on ? " selected" : ""}>${r}</option>`).join("")}</select>`;

// The sections of a repo's settings, one shown at a time: `/r/<repo>/settings/<section>`.
const SETTINGS = [["general", "General", "settings"], ["collaborators", "Collaborators", "users"], ["tokens", "Agents and tokens", "key"], ["security", "Security", "lock"], ["secrets", "Secrets", "proofs"], ["log", "Audit log", "log"], ["danger", "Danger zone", "alert"]];

page(repoPage(String.raw`\/settings(?:\/([a-z]+))?`), async (el, [repo, part], q, n) => {
  const tabs = await repoFrame(el, repo, "settings", [["Settings"]]);
  const soft = (p) => p.catch((e) => ({ error: e }));
  const [s, secrets, cfg, log] = await Promise.all([soft(api(`${R(repo)}/settings`)), soft(api(`${R(repo)}/secrets`)), soft(api(`${R(repo)}/config`)), soft(api(`${R(repo)}/settings/audit?limit=30`))]);
  if (!current(n)) return;
  if (s.error) {
    el.innerHTML = tabs + (s.error.status === 403 ? empty("Settings are its admins'", esc(s.error.message), [], { icon: "lock" }) : failed(s.error));
    return;
  }
  const on = SETTINGS.find(([k]) => k === part)?.[0] || "general";
  // Reached by a name it had: the address says the one it has now.
  if (s.name !== repo) history.replaceState(null, "", location.pathname.replace(P(repo), P(s.name)));
  const name = s.name, flag = `--repo ${name}`;
  const or = (x, f) => (x && x.error ? `<p class="soft small">${esc(x.error.message)}</p>` : f(x));
  const said = (id) => `<p class="said" id="${id}" role="status"></p>`;
  const sec = s.security || {};
  const steps = cfg.config?.check?.steps || [];
  const toks = s.tokens || [];
  const state = s.deleted
    ? `<div class="notice red"><strong>Deleted</strong> ${when(s.deleted.at)}${s.deleted.by ? ` by ${esc(s.deleted.by)}` : ""}: it answers as missing (clone, tasks, landings). Its data goes ${when(s.deleted.purge_at)}. <button class="primary sm" data-act="restore" style="margin-left:var(--avp-space-2)">Restore</button></div>`
    : s.archived ? `<div class="notice"><strong>Archived</strong> ${when(s.archived.at)}${s.archived.by ? ` by ${esc(s.archived.by)}` : ""}: read-only, nothing opens nor lands. <button class="sm" data-act="unarchive" style="margin-left:var(--avp-space-2)">Unarchive</button></div>` : "";
  const head = (title, lead) => `<h2>${esc(title)}</h2><p class="lead">${lead}</p>`;
  const cli = (c) => `<div class="cfoot"><span>${icon("terminal")}</span><span class="grow">Or from a terminal, or an agent (avp_repo): <code>${esc(c)}</code></span><button type="button" class="ghost sm" data-copy="${esc(c)}">Copy</button></div>`;
  const SECTION = {
    general: () => `${head("General", "Its name, what it is, who reads it.")}
      <div class="card boxed"><div class="cbody"><form id="general-form">
        <label>Name<input name="name" value="${esc(s.repo.name)}" required></label>
        <label>Description<input name="description" value="${esc(s.description)}" maxlength="300" placeholder="What this repo is, in a line"></label>
        ${s.public_allowed || s.visibility === "public" ? `<label>Visibility<select name="visibility"><option value="private"${s.visibility === "private" ? " selected" : ""}>Private: only those given a role reach it</option><option value="public"${s.visibility === "public" ? " selected" : ""}>Public: anyone reads and clones it, nobody writes without a role</option></select></label>` : `<div style="margin-bottom:var(--avp-space-4)"><span class="label" style="margin-bottom:6px">Visibility</span><p style="margin:0">Private <span class="soft small">(this server keeps every repo private)</span></p></div>`}
        <button class="primary">Save</button>${said("general-said")}</form></div>
      <div class="cfoot" style="display:block"><dl class="facts"><dt>Clone</dt><dd class="mono">${esc(cloneUrl(name))}</dd>${s.names_before?.length ? `<dt>Also answers to</dt><dd>${s.names_before.map(esc).join(", ")} <span class="soft">(renamed or moved: old addresses and clones keep working)</span></dd>` : ""}<dt>Created</dt><dd>${dateOf(s.repo.created_at)}</dd></dl></div></div>`,
    collaborators: () => `${head("Collaborators", `read ${ROLE_DOES.read}; write ${ROLE_DOES.write}; admin ${ROLE_DOES.admin}.${s.org ? ` Members of ${esc(s.org)} reach it by their org role (member: write, admin and owner: admin); a role given here comes first.` : ""}`)}
      <div class="card boxed"><div class="chead"><h3>Invite a person</h3><span class="soft">They get the role at once.</span></div><div class="cbody">
        <form id="invite" class="row"><input name="person" placeholder="Name or email of a person signed up here" required aria-label="Name or email">${roleSelect("role", "write")}<button class="primary">Invite</button></form>${said("collaborators-said")}</div>
        ${cli(`avp repo invite <name or email> --role write ${flag}`)}</div>
      <div class="table" style="margin-top:var(--avp-space-4)">${s.collaborators.length ? `<table><tr><th>Person</th><th>Role</th><th class="end"></th></tr>${s.collaborators.map((c) => `<tr><td><span style="display:inline-flex;align-items:center;gap:var(--avp-space-2)">${avatar(c.person)}${esc(c.person)}</span>${c.org_role ? ` <span class="soft small">${esc(c.org_role)} of ${esc(s.org)}</span>` : ""}</td>
        <td>${c.via === "repo" ? roleSelect(`role-${esc(c.person)}`, c.role) : `<span class="pill">${esc(c.role)}</span> <span class="soft small">by ${esc(s.org)}</span>`}</td>
        <td class="end">${c.via === "repo" ? `<button class="ghost sm" data-act="remove" data-person="${esc(c.person)}">Remove</button>` : ""}</td></tr>`).join("")}</table>` : empty("Nobody yet", "Invite a person: they reach it with the role you give.", [], { icon: "users" })}</div>`,
    tokens: () => `${head("Agents and tokens", "A token made here reaches this repo alone, to read or to write, for an agent or a CI: never its settings, never an approval. Shown once, never again. Each person's own tokens reach it too, as them: yours are listed here, theirs only in their own settings.")}
      <div class="card boxed"><div class="chead"><h3>Connect an agent</h3></div><div class="cbody"><p class="soft small" style="margin:0 0 var(--avp-space-3)">A person's agent signs in as them (<code>avp login</code>): no token to make. One click opens Cursor or VS Code with it; one line starts the others.</p>${agentTiles(name)}<a class="button" href="${connectHref(name)}">${icon("plug")}Connect an agent</a></div></div>
      <div class="card boxed"><div class="chead"><h3>New token</h3><span class="soft">For a CI or a machine with no person</span></div><div class="cbody">
        <form id="token" class="token-form" style="grid-template-columns:minmax(0,1fr) 120px 130px auto"><label>For<input name="name" placeholder="ci, an agent" required autocomplete="off"></label><label>Role${roleSelect("role", "read", ["read", "write"]).replace(' style="width:110px"', "")}</label>
          <label>Expires<select name="expires"><option value="90d">90 days</option><option value="30d">30 days</option><option value="7d">7 days</option><option value="never">No end</option></select></label><button class="primary">Create</button></form>
        <div id="token-made"></div>${said("tokens-said")}</div>${cli(`avp repo token create --name ci --role read ${flag}`)}</div>
      <div class="table" style="margin-top:var(--avp-space-4)">${toks.length ? `<table><tr><th>Name</th><th>Owner</th><th>Scope</th><th>Last used</th><th class="end"></th></tr>${toks.map((t) => `<tr><td><strong>${esc(t.name)}</strong><div>${idChip(t.id)}</div></td><td>${esc(t.person)}</td><td><span class="pill">${esc(t.scope)}</span></td><td>${t.used_at ? timeAgo(t.used_at) : `<span class="soft">never</span>`}${t.expires_at ? `<div class="soft small">ends ${when(t.expires_at)}</div>` : ""}</td>
        <td class="end">${t.revocable ? `<button class="danger sm" data-act="revoke" data-id="${esc(t.id)}">Revoke</button>` : ""}</td></tr>`).join("")}</table>` : empty("No token", "None reaches it but its people's own.", [], { icon: "key" })}</div>`,
    security: () => `${head("Security", "A change to a guarded path waits for a person's approval before it lands. Read only here: admins change guards with <code>avp guard</code>, and avp.toml is guarded itself.")}
      <div class="card"><dl class="facts"><dt>Guarded</dt><dd class="mono">${[...(sec.always_guarded || []), ...(sec.guarded || [])].filter((g) => g !== "trame.toml").map(esc).join("<br>") || "nothing"}</dd>
        <dt>Approved by</dt><dd>${(sec.approvers || []).map(esc).join(", ") || `<span class="soft">no one with write yet</span>`} <span class="soft small">(write or admin; a token never approves)</span></dd>
        <dt>Landing rule</dt><dd>${esc(sec.landing)}${sec.landing === "proof" ? ` <span class="soft small">(a green proof lands)</span>` : ""}</dd></dl></div>
      <div class="card boxed"><div class="chead"><h3>Checks</h3><span class="soft">What every landing runs on the head (${esc(cfg.file || "avp.toml")}, guarded)</span></div>${steps.length ? `<div class="list">${steps.map((x) => `<div class="item"><span class="glyph soft">${icon("proofs")}</span><span class="main"><span class="title">${esc(x.name)}</span><span class="meta mono">${esc(x.run || "")}</span></span></div>`).join("")}</div>` : `<div class="cbody soft">No check: every landing goes in unproven.</div>`}</div>`,
    secrets: () => `${head("Secrets", "Their names only: values never leave the server, a proof's steps get the ones they name.")}
      <div class="card boxed">${or(secrets, (ss) => ss.length ? `<div class="list">${ss.map((x) => `<div class="item"><span class="soft">${icon("key")}</span><span class="main mono">${esc(x.name || x)}</span>${x.set_at ? `<span class="side">set ${timeAgo(x.set_at)}</span>` : ""}</div>`).join("")}</div>` : empty("No secret", "Set one from a terminal: the value goes to the server, never to the page.", [], { icon: "lock" }))}${cli(`avp secret set NAME ${flag} < value.txt`)}</div>`,
    log: () => `${head("Audit log", "Who changed a setting, a role, a token, a guard or a secret, and when.")}
      <div class="card boxed">${or(log, (evs) => evs.length ? `<div class="list">${evs.map((e) => `<div class="item">${avatar(e.by || "?")}<span class="main"><span class="title"><strong>${esc(e.by || "someone")}</strong> ${esc(e.what)}</span></span><span class="side">${timeAgo(e.at)}</span></div>`).join("")}</div>` : empty("No change yet", "Every change to these settings shows up here.", [], { icon: "log" }))}${cli(`avp repo log ${flag}`)}</div>`,
    danger: () => `${head("Danger zone", "What cannot be taken back in a click: each one asks first.")}
      <div class="card boxed danger-zone">
      ${s.cloud && (s.spaces || []).some((x) => x !== s.repo.space) ? `<form id="transfer" class="danger-row"><div class="grow"><strong>Transfer</strong><span>Its roles stay, its old address keeps answering, its proof minutes become the new space's.</span></div><select name="to" style="width:auto;min-width:160px" aria-label="To">${(s.spaces || []).filter((x) => x !== s.repo.space).map((x) => `<option>${esc(x)}</option>`).join("")}</select><button class="danger">Transfer</button></form>` : ""}
      ${s.deleted ? "" : `<div class="danger-row"><div class="grow"><strong>${s.archived ? "Unarchive" : "Archive"}</strong><span>${s.archived ? "Archived: read-only. Unarchived, tasks open and land again." : "Read-only: every task opened or landed is refused with a clear message."}</span></div><button class="danger" data-act="${s.archived ? "unarchive" : "archive"}">${s.archived ? "Unarchive" : "Archive"}</button></div>`}
      ${s.deleted ? "" : `<form id="delete" class="danger-row"><div class="grow"><strong>Delete</strong><span>Type <strong class="mono" style="display:inline">${esc(s.repo.name)}</strong> to confirm. Restorable by an admin for 7 days, then its data goes.</span></div><input name="confirm" autocomplete="off" aria-label="Its name, to confirm" placeholder="${esc(s.repo.name)}" style="width:200px"><button class="danger" disabled>Delete</button></form>`}
      </div>${said("danger-said")}`,
  };
  el.innerHTML = tabs + `<div class="settings"><nav class="subnav" aria-label="Settings">${SETTINGS.map(([k, label, ic], i) => `${i === SETTINGS.length - 1 ? "<hr>" : ""}<a href="${P(name)}/settings${k === "general" ? "" : "/" + k}" class="${k === on ? "on" : ""}${k === "danger" ? " danger" : ""}"${k === on ? ` aria-current="page"` : ""}>${icon(ic)}${label}</a>`).join("")}</nav>
    <section id="${on}" style="max-width:820px">${state}${SECTION[on]()}</section></div>`;

  // Every change: the server's answer says what it did; the page is drawn again from the server.
  const send = async (rest, body, out) => {
    try {
      const r = await api(`${R(name)}/settings${rest}`, { method: "POST", body });
      if (r.said) toast(r.said.split(":")[0]);
      return r;
    } catch (e) { $(out).textContent = e.message; return null; }
  };
  const again = (r, to = name) => { if (!r) return; if (to !== name) go(`${P(to)}/settings${on === "general" ? "" : "/" + on}`, true); else render(); };
  $("general-form")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target), body = {};
    if (f.get("name").trim() !== s.repo.name) body.name = f.get("name").trim();
    if (f.get("description").trim() !== s.description) body.description = f.get("description").trim();
    if (f.has("visibility") && f.get("visibility") !== s.visibility) body.visibility = f.get("visibility");
    if (!Object.keys(body).length) { $("general-said").textContent = "Nothing changed."; return; }
    const r = await send("/general", body, "general-said");
    if (!r) return;
    // Renamed, the page moves to the new name; else it stays as typed, and says it is saved.
    if (body.name) return again(r, r.name);
    Object.assign(s, body);
    ABOUT.delete(name);
    $("general-said").textContent = "Saved.";
  });
  $("general-form")?.addEventListener("input", () => { $("general-said").textContent = ""; });
  $("invite")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    again(await send("/collaborators", { person: f.get("person").trim(), role: f.get("role") }, "collaborators-said"));
  });
  el.querySelectorAll('select[name^="role-"]').forEach((sel) => sel.addEventListener("change", async () => {
    again(await send("/collaborators", { person: sel.name.slice(5), role: sel.value }, "collaborators-said"));
  }));
  $("token")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    const r = await send("/tokens", { name: f.get("name").trim(), role: f.get("role"), expires: f.get("expires") }, "tokens-said");
    if (!r) return;
    await render();
    $("token-made").innerHTML = `<div class="notice" style="margin-top:var(--avp-space-3)"><strong>Copy it now: it is never shown again.</strong> ${esc(r.said.split(": ").slice(1).join(": ") || r.said)}${cmd(r.token)}</div>`;
  });
  const del = $("delete");
  if (del) {
    del.confirm.addEventListener("input", () => { del.querySelector("button").disabled = del.confirm.value.trim() !== s.repo.name; });
    del.addEventListener("submit", async (ev) => {
      ev.preventDefault();
      const r = await send("/delete", { confirm: del.confirm.value.trim() }, "danger-said");
      if (!r) return;
      ABOUT.delete(name);
      toast(`${name} deleted. An admin restores it from ${location.pathname} for 7 days.`);
      go("/");
    });
  }
  $("transfer")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const to = new FormData(ev.target).get("to");
    if (!to || !confirm(`Move ${name} to ${to}?`)) return;
    const r = await send("/transfer", { to }, "danger-said");
    again(r, r?.name);
  });
  el.querySelectorAll("[data-act]").forEach((b) => b.addEventListener("click", async () => {
    const act = b.dataset.act;
    if (act === "remove") again(await send("/collaborators", { person: b.dataset.person, role: "none" }, "collaborators-said"));
    if (act === "revoke" && confirm("Revoke this token? Whatever uses it stops at once.")) again(await send(`/tokens/${enc(b.dataset.id)}/revoke`, {}, "tokens-said"));
    if (act === "archive" && !confirm(`Archive ${name}? Nothing opens or lands on it until it is unarchived.`)) return;
    if (act === "archive" || act === "unarchive") ABOUT.delete(name);
    if (act === "archive" || act === "unarchive") again(await send("/archive", { archived: act === "archive" }, "danger-said"));
    if (act === "restore") ABOUT.delete(name);
    if (act === "restore") again(await send("/restore", {}, "danger-said"));
  }));
});

// ---- changing code from the browser: edit, add, upload, delete ----
// A save is a task, as an agent's: opened on the position the person read, its files written, then
// landed, which proves it on the head of the line. The message is the task's goal. No verb of its own.

// Bigger than these, files go through the CLI: a browser holds what it sends in memory.
const UPLOAD_FILE_MAX = 10 * 1024 * 1024;
const UPLOAD_TOTAL_MAX = 50 * 1024 * 1024;
const UPLOAD_FILES_MAX = 1000;
const encPath = (path) => path.split("/").map(enc).join("/");
const clockOf = (s) => `${Math.floor(s / 60)}:${String(Math.floor(s % 60)).padStart(2, "0")}`;
const mb = (n) => `${(n / 1024 / 1024).toFixed(n < 10 * 1024 * 1024 ? 1 : 0)} MB`;
const size = (n) => (n < 1024 ? `${n} B` : n < 1024 * 1024 ? `${(n / 1024).toFixed(1)} KB` : mb(n));

// Whether to offer the changes: the server decides, this only spares a person a button that a 403
// answers. A repo /me does not name may be the person's through an organization: offered.
function mayWrite(repo) {
  if (ABOUT_NOW.get(repo)?.archived) return false;
  if (!ME || !ME.accounts || ME.admin) return true;
  const short = (r) => r.split("/").pop();
  const role = (ME.roles || []).find((x) => x.repo === repo || (short(x.repo) === short(repo) && (!x.repo.includes("/") || !repo.includes("/"))));
  return !role || role.role !== "read";
}
const changeHref = (repo, path, what) => `${P(repo)}/code${path ? "/" + encPath(path) : ""}?${what}=1`;

function textLines(text) {
  const lines = text.split("\n");
  if (lines.length > 1 && lines[lines.length - 1] === "") lines.pop();
  return text === "" ? [] : lines;
}

// A unified patch of two texts, for the preview (drawn by diffView, as a position's). The common
// ends are cut first; a middle too big to compare line by line shows as replaced whole.
function linePatch(before, after) {
  const a = textLines(before), b = textLines(after);
  let s = 0;
  while (s < a.length && s < b.length && a[s] === b[s]) s++;
  let e = 0;
  while (e < a.length - s && e < b.length - s && a[a.length - 1 - e] === b[b.length - 1 - e]) e++;
  const A = a.slice(s, a.length - e), B = b.slice(s, b.length - e);
  const mid = [];
  if (A.length * B.length > 4e6) {
    A.forEach((l) => mid.push(["-", l]));
    B.forEach((l) => mid.push(["+", l]));
  } else {
    const w = B.length + 1, L = new Uint32Array((A.length + 1) * w);
    for (let i = A.length - 1; i >= 0; i--) for (let j = B.length - 1; j >= 0; j--) L[i * w + j] = A[i] === B[j] ? L[(i + 1) * w + j + 1] + 1 : Math.max(L[(i + 1) * w + j], L[i * w + j + 1]);
    let i = 0, j = 0;
    while (i < A.length || j < B.length) {
      if (i < A.length && j < B.length && A[i] === B[j]) { mid.push([" ", A[i]]); i++; j++; }
      // What goes before what comes, as a unified diff reads.
      else if (i < A.length && (j === B.length || L[(i + 1) * w + j] >= L[i * w + j + 1])) mid.push(["-", A[i++]]);
      else mid.push(["+", B[j++]]);
    }
  }
  const ops = [...a.slice(0, s).map((l) => [" ", l]), ...mid, ...a.slice(a.length - e).map((l) => [" ", l])];
  // Hunks: each change with three lines around it, the close ones merged.
  const at = [];
  let ai = 1, bi = 1;
  for (const [k] of ops) { at.push([ai, bi]); if (k !== "+") ai++; if (k !== "-") bi++; }
  const out = [];
  let k = 0;
  while (k < ops.length) {
    if (ops[k][0] === " ") { k++; continue; }
    const start = Math.max(0, k - 3);
    let end = k;
    for (let j = k; j < ops.length && j <= end + 6; j++) if (ops[j][0] !== " ") end = j;
    const stop = Math.min(ops.length, end + 4);
    const part = ops.slice(start, stop);
    const na = part.filter(([x]) => x !== "+").length, nb = part.filter(([x]) => x !== "-").length;
    out.push(`@@ -${na ? at[start][0] : at[start][0] - 1},${na} +${nb ? at[start][1] : at[start][1] - 1},${nb} @@`, ...part.map(([x, l]) => x + l));
    k = stop;
  }
  return out.join("\n");
}

async function fileText(repo, entry) {
  const r = await fetch(`${R(repo)}/objects/${entry.blob}`, { headers: LIVE });
  if (!r.ok) throw new ApiError(r.status, `The file could not be read: ${r.status}`);
  return new TextDecoder().decode(await r.arrayBuffer());
}

// The pages of a change, on the head only: `?edit`, `?delete` on a file, `?new`, `?upload` on a folder.
async function changePage(el, tabs, repo, path, tree, head, what, n) {
  const entry = tree.files[path];
  const isFile = !!entry;
  if ((what === "edit" || what === "delete") !== isFile) throw new ApiError(404, isFile ? `${path} is a file.` : `No file ${path} at position ${head.position}.`);
  const dir = isFile ? path.split("/").slice(0, -1).join("/") : path;
  const title = { edit: "Editing", delete: "Deleting", new: "A new file in", upload: "Uploading to" }[what];
  el.innerHTML = tabs + `<div class="bar"><div class="grow">${pathCrumbs(repo, path, head.position, head.position)}</div><span class="small soft">${title} ${esc(path || repo)} on position ${head.position}</span></div><div id="change"></div><div id="review"></div>`;
  const box = $("change");
  const back = codeHref(repo, path, head.position, head.position);
  // Where a landing leads: the file edited or added, the folder of a delete or an upload.
  const ctx = { repo, head, tree, box, review: $("review"), back, done: what === "edit" ? back : codeHref(repo, dir, head.position, head.position), state: {} };
  if (what === "edit" || what === "new") {
    const before = isFile ? await fileText(repo, entry) : "";
    if (!current(n)) return;
    if (before.slice(0, 8000).includes("\0")) throw new ApiError(400, `${path} is a binary file: upload its new version from its folder.`);
    return editor(ctx, path, dir, before, entry);
  }
  if (what === "delete") {
    const before = await fileText(repo, entry).catch(() => "");
    if (!current(n)) return;
    box.innerHTML = "";
    const binary = before.slice(0, 8000).includes("\0");
    return review(ctx, [{ path, change: "deleted", binary, patch: binary ? "" : linePatch(before, ""), delete: true }], `Delete ${path}`, null);
  }
  return uploader(ctx, dir);
}

// The editor: a textarea in the file's monospace, its line numbers beside it.
function editor(ctx, path, dir, before, entry) {
  const crlf = before.includes("\r\n");
  ctx.box.innerHTML = `<div class="file"><div class="fhead">${entry ? `<span class="grow mono">${esc(path)}</span>` : `<span class="mono soft">${esc(dir ? dir + "/" : "")}</span><input id="name" class="mono name" placeholder="name-of-file.txt" autocomplete="off" spellcheck="false" aria-label="File name">`}<span class="soft small" id="count"></span></div>
    <div class="editor"><pre class="gutter" id="gutter" aria-hidden="true"></pre><textarea id="text" wrap="off" spellcheck="false" autocapitalize="off" aria-label="Content of the file"></textarea></div></div>
    <p class="small" id="err" role="alert"></p>
    <div class="bar stick"><span class="grow"></span><a class="button" href="${ctx.back}">Cancel</a><button class="primary" id="go">Review the change</button></div>`;
  const text = $("text"), gutter = $("gutter");
  text.value = crlf ? before.replace(/\r\n/g, "\n") : before;
  let drawn = 0;
  const numbers = () => {
    const lines = Math.max(1, textLines(text.value).length);
    if (lines !== drawn) { gutter.textContent = Array.from({ length: lines }, (_, i) => i + 1).join("\n"); drawn = lines; }
    $("count").textContent = plural(lines, "line");
  };
  numbers();
  text.addEventListener("input", numbers);
  text.addEventListener("scroll", () => { gutter.scrollTop = text.scrollTop; });
  // Tab indents, as in any editor; Escape then Tab leaves the field for the keyboard's sake.
  let escaped = false;
  text.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") { escaped = true; return; }
    if (ev.key === "Tab" && !ev.shiftKey && !escaped) { ev.preventDefault(); document.execCommand("insertText", false, "  ") || text.setRangeText("  ", text.selectionStart, text.selectionEnd, "end"); numbers(); }
    escaped = false;
  });
  (entry ? text : $("name")).focus();
  $("go").addEventListener("click", () => {
    const name = entry ? path : `${dir ? dir + "/" : ""}${$("name").value.trim().replace(/^\/+/, "")}`;
    const err = entry ? "" : !$("name").value.trim() ? "Name the file first." : !validPath(name) ? `${name} is not a path a file can have: no empty part, no "." or "..".` : ctx.tree.files[name] ? `${name} already exists: open it and edit it.` : ctx.tree.dirs.has(name) ? `${name} is a folder.` : "";
    $("err").textContent = err;
    if (err) return;
    const after = crlf ? text.value.replace(/\n/g, "\r\n") : text.value;
    if (entry && after === before) { $("err").textContent = "Nothing changed yet."; return; }
    ctx.box.hidden = true;
    ctx.done = codeHref(ctx.repo, name, ctx.head.position, ctx.head.position);
    review(ctx, [{ path: name, change: entry ? "modified" : "added", patch: linePatch(before, after), body: after }], entry ? `Update ${name}` : `Add ${name}`, () => { ctx.box.hidden = false; ctx.review.innerHTML = ""; text.focus(); });
  });
}
const validPath = (p) => p && !p.startsWith("/") && !/[\\\0]/.test(p) && p.split("/").every((c) => c && c !== "." && c !== "..");

// Uploads: dropped files and folders, or picked; each lands at its path under the folder.
function uploader(ctx, dir) {
  const picked = new Map();
  ctx.box.innerHTML = `<div class="drop" id="drop"><strong>Drop files or a folder here</strong><span class="soft small">or</span>
      <span class="row"><label class="button">Choose files<input type="file" id="files" multiple hidden></label><label class="button">Choose a folder<input type="file" id="folder" webkitdirectory multiple hidden></label></span>
      <span class="soft small">Up to ${mb(UPLOAD_FILE_MAX)} a file and ${mb(UPLOAD_TOTAL_MAX)} in all. Into <span class="mono">${esc(dir || "the root")}</span>.</span></div>
    <p class="small" id="err" role="alert"></p><div id="picked"></div>
    <div class="bar" style="margin-top:var(--avp-space-4)"><span class="grow"></span><a class="button" href="${ctx.back}">Cancel</a><button class="primary" id="go" disabled>Review the change</button></div>`;
  const draw = () => {
    const files = [...picked.values()];
    const total = files.reduce((s, f) => s + f.file.size, 0);
    $("picked").innerHTML = files.length ? `<div class="list">${files.map((f) => `<div class="item"><span class="main"><span class="title mono">${esc(f.path)}</span></span><span class="side">${ctx.tree.files[f.path] ? "replaces the file" : "new"} · ${size(f.file.size)}</span><button class="link small" data-drop="${esc(f.path)}">Remove</button></div>`).join("")}
      <div class="item head"><span class="main">${plural(files.length, "file")}</span><span class="side">${size(total)} of ${mb(UPLOAD_TOTAL_MAX)}</span></div></div>` : "";
    $("go").disabled = !files.length;
  };
  const add = (list) => {
    const refused = [];
    let total = [...picked.values()].reduce((s, f) => s + f.file.size, 0);
    for (const [rel, file] of list) {
      const path = `${dir ? dir + "/" : ""}${rel.replace(/^\/+/, "")}`;
      if (!validPath(path)) refused.push(`${rel}: not a path a file can have`);
      else if (file.size > UPLOAD_FILE_MAX) refused.push(`${rel} is ${mb(file.size)}, past the ${mb(UPLOAD_FILE_MAX)} a file may weigh from the browser`);
      else if (picked.size >= UPLOAD_FILES_MAX) refused.push(`${rel}: ${UPLOAD_FILES_MAX} files at most at once`);
      else if (total + file.size - (picked.get(path)?.file.size || 0) > UPLOAD_TOTAL_MAX) refused.push(`${rel}: past the ${mb(UPLOAD_TOTAL_MAX)} one save may carry`);
      else if (ctx.tree.dirs.has(path)) refused.push(`${rel}: a folder has that path`);
      else { total += file.size - (picked.get(path)?.file.size || 0); picked.set(path, { path, file }); }
    }
    $("err").innerHTML = refused.length ? `Left out: ${refused.slice(0, 5).map(esc).join("; ")}${refused.length > 5 ? `; and ${refused.length - 5} more` : ""}. Bigger files go through the CLI: <code>avp open</code> in a folder of the repo, then <code>avp land</code>.` : "";
    draw();
  };
  const pick = (input) => input.addEventListener("change", () => { add([...input.files].map((f) => [f.webkitRelativePath || f.name, f])); input.value = ""; });
  pick($("files"));
  pick($("folder"));
  $("picked").addEventListener("click", (ev) => { const b = ev.target.closest("[data-drop]"); if (b) { picked.delete(b.dataset.drop); draw(); } });
  const drop = $("drop");
  drop.addEventListener("dragover", (ev) => { ev.preventDefault(); drop.classList.add("over"); });
  drop.addEventListener("dragleave", () => drop.classList.remove("over"));
  drop.addEventListener("drop", async (ev) => {
    ev.preventDefault();
    drop.classList.remove("over");
    const entries = [...ev.dataTransfer.items].map((i) => i.webkitGetAsEntry?.()).filter(Boolean);
    add(entries.length ? await walk(entries) : [...ev.dataTransfer.files].map((f) => [f.name, f]));
  });
  $("go").addEventListener("click", () => {
    const files = [...picked.values()];
    ctx.box.hidden = true;
    const what = files.length === 1 ? files[0].path : `${plural(files.length, "file")} to ${dir || "the root"}`;
    review(ctx, files.map((f) => ({ path: f.path, change: ctx.tree.files[f.path] ? "modified" : "added", binary: true, size: f.file.size, body: f.file })), `Upload ${what}`, () => { ctx.box.hidden = false; ctx.review.innerHTML = ""; });
  });
}

// The files under dropped entries, with their paths from the drop (a folder's name first).
async function walk(entries) {
  const out = [];
  const read = (dir) => new Promise((ok, no) => {
    const reader = dir.createReader(), all = [];
    const more = () => reader.readEntries((batch) => (batch.length ? (all.push(...batch), more()) : ok(all)), no);
    more();
  });
  const go = async (entry) => {
    if (entry.isFile) out.push([entry.fullPath.replace(/^\//, ""), await new Promise((ok, no) => entry.file(ok, no))]);
    else if (entry.isDirectory) for (const e of await read(entry)) await go(e);
  };
  for (const e of entries) await go(e);
  return out;
}

// The preview of what lands, the message, then the landing.
function review(ctx, files, goal, back) {
  const list = files.map((f) => ({ path: f.path, change: f.change, binary: f.binary, patch: f.binary ? "" : f.patch }));
  const sized = files.filter((f) => f.size != null);
  ctx.review.innerHTML = `<h2>Review the change</h2>${sized.length ? `<div class="list" style="margin-bottom:var(--avp-space-4)">${sized.map((f) => `<div class="item"><span class="main"><span class="title mono">${esc(f.path)}</span></span><span class="pill">${f.change === "added" ? "added" : "replaced"}</span><span class="side">${size(f.size)}</span></div>`).join("")}</div>` : diffView(list)}
    <form id="save" class="card savebox"><label>Message <input name="goal" required maxlength="500" value="${esc(ctx.goal || goal)}"></label>
      <p class="small soft">This message names the change in the history.</p>
      <div class="bar" style="margin:0"><span class="grow"></span>${back ? `<button type="button" id="back">Back to editing</button>` : `<a class="button" href="${ctx.back}">Cancel</a>`}<button class="primary" id="land">Land it</button></div></form>
    <div id="landing"></div>`;
  $("back")?.addEventListener("click", back);
  $("save").addEventListener("submit", (ev) => {
    ev.preventDefault();
    const goalNow = new FormData(ev.target).get("goal").trim();
    if (!goalNow) return;
    ctx.goal = goalNow;
    ev.target.querySelectorAll("button, input").forEach((x) => (x.disabled = true));
    land(ctx, files, goalNow, back);
  });
  ctx.review.scrollIntoView({ block: "start", behavior: "smooth" });
}

// One save: the task opened (once: a red proof fixed lands again on the same task), its files
// written, its landing followed until the line answers.
async function land(ctx, files, goal, back) {
  const { repo, head, state } = ctx;
  const box = $("landing");
  const steps = await api(`${R(repo)}/config`).then((c) => (c.config?.check?.steps || []).map((s) => s.name)).catch(() => []);
  const started = Date.now();
  let phase = "Opening a task on position " + head.position;
  const drawLive = () => {
    box.innerHTML = `<div class="card landing"><div class="bar" style="margin:0"><span class="dot run"></span><strong class="grow">${esc(phase)}</strong><span class="soft small mono">${clockOf((Date.now() - started) / 1000)}</span></div>
      ${steps.length ? `<div class="list steps" style="margin-top:var(--avp-space-3)">${steps.map((s) => `<div class="item"><span class="dot wait"></span><span class="main"><span class="title">${esc(s)}</span></span><span class="side">waiting</span></div>`).join("")}</div>` : ""}</div>`;
  };
  drawLive();
  const tick = setInterval(drawLive, 1000);
  const leaving = landing();
  const stop = (html) => { clearInterval(tick); leaving(); box.innerHTML = html; };
  try {
    // Again after a red proof: the task's writes start over from what the page holds now.
    if (state.task) {
      await api(`${R(repo)}/tasks/${enc(state.task)}/goal`, { method: "POST", body: { goal } });
      await api(`${R(repo)}/tasks/${enc(state.task)}/undo`, { method: "POST", body: { to: 0 } });
    } else {
      const t = await api(`${R(repo)}/tasks?base=${head.position}`, { method: "POST", body: { goal, scope: files.map((f) => f.path), actor: { person: ME?.person || "", agent: "", session: "" } } });
      state.task = t.id;
    }
    const task = state.task;
    let i = 0;
    for (const f of files) {
      phase = files.length > 1 ? `Writing ${++i} of ${files.length}: ${f.path}` : `Writing ${f.path}`;
      drawLive();
      const out = f.delete
        ? await api(`${R(repo)}/tasks/${enc(task)}/writes`, { method: "POST", body: { path: f.path, kind: "delete" } })
        : await put(`${R(repo)}/tasks/${enc(task)}/files/${encPath(f.path)}`, f.body);
      if (out.lease === "refused") throw new ApiError(409, `${f.path} is held by another task (${out.previous_holder}) and this repo's leases are strict: try again once it lands.`);
    }
    phase = steps.length ? `Proving on the head of the line: ${plural(steps.length, "step")}` : "Landing on the head of the line";
    drawLive();
    const { status, body } = await landRequest(repo, task);
    stop(await landed(ctx, task, status === 200 ? body : null, status === 409 ? body : null, back));
  } catch (e) {
    stop(`<div class="card landing red"><strong>Not landed</strong><p>${esc(e.message)}</p>${state.task ? taskLink(repo, state.task) : ""}<div class="bar" style="margin:0"><span class="grow"></span>${back ? `<button id="again">Back to editing</button>` : ""}<a class="button" href="${ctx.back}">Leave</a></div></div>`);
    $("again")?.addEventListener("click", back);
  }
  $("drop-task")?.addEventListener("click", async (ev) => {
    ev.target.disabled = true;
    await api(`${R(repo)}/tasks/${enc(state.task)}/abandon`, { method: "POST" }).catch(() => {});
    go(ctx.back);
  });
  $("fix")?.addEventListener("click", back);
}

// A task's land, its answer kept alive while it proves: 200 landed, 409 not landed and why.
async function landRequest(repo, task) {
  const r = await fetch(`${R(repo)}/tasks/${enc(task)}/land`, { method: "POST", headers: { ...LIVE, "x-avp-keepalive": "1" } });
  const text = (await r.text()).trim();
  let body = null;
  try { body = JSON.parse(text); } catch { throw new ApiError(r.status, text || `${r.status}`); }
  let status = r.status;
  if (r.ok && body && body.status != null && "body" in body) ({ status, body } = body);
  if (status >= 400 && status !== 409) throw new ApiError(status, body?.error || `${status}`);
  return { status, body };
}

// While a landing is followed, closing or leaving the page asks first: the task stays open
// otherwise, to be landed again or abandoned from the repo's Tasks. Returns what ends it.
function landing() {
  const ask = (ev) => { ev.preventDefault(); ev.returnValue = ""; };
  addEventListener("beforeunload", ask);
  return () => removeEventListener("beforeunload", ask);
}

async function put(path, body) {
  const r = await fetch(path, { method: "PUT", headers: { ...LIVE, "content-type": "application/octet-stream" }, body });
  const text = await r.text();
  let out = null;
  try { out = JSON.parse(text); } catch { out = text; }
  if (!r.ok) throw new ApiError(r.status, out?.error || (typeof out === "string" && out) || `${r.status}`);
  return out;
}
const taskLink = (repo, task) => `<p class="small soft">Task <a class="mono" href="${P(repo)}/tasks/${enc(task)}">${esc(task)}</a></p>`;

// What the line said: the steps of the proof, then the position, or what stops it and what to do.
async function landed(ctx, task, ok, not, back) {
  const { repo } = ctx;
  const proof = ok ? await api(`${R(repo)}/proofs/${enc(ok.proof)}`).catch(() => null) : not?.proof;
  const stepRows = proof?.steps?.length ? `<div class="list steps" style="margin-top:var(--avp-space-3)">${proof.steps.map((s) => `<div class="item"><span class="dot ${s.exit === 0 ? "green" : "red"}"></span><span class="main"><span class="title">${esc(s.name)}</span><span class="meta mono">${esc(s.command || "")}</span></span><span class="side">${s.exit === 0 ? "" : `exit ${s.exit} · `}${dur(s.seconds)}</span></div>`).join("")}</div>` : "";
  const leave = `<a class="button" href="${ctx.back}">Leave</a>`;
  if (ok) {
    TREES.clear();
    OPEN.delete(repo);
    return `<div class="card landing green"><div class="bar" style="margin:0"><span class="dot green"></span><strong class="grow">Landed at <a href="${P(repo)}/p/${ok.position}">position ${ok.position}</a></strong>${proof ? proofPill(proof.verdict, proof.steps) : ""}</div>${stepRows}
      <div class="bar" style="margin:var(--avp-space-3) 0 0"><span class="grow small soft">${ok.replayed ? "The repo moved while you worked: your change was replayed on the latest version and checked there." : proof && !proof.steps?.length ? "No checks yet: add them to avp.toml." : "Checked on the latest version."}</span><a class="button" href="${P(repo)}/p/${ok.position}">See the position</a><a class="button primary" href="${ctx.done}">Back to the code</a></div></div>`;
  }
  const fix = back ? `<button class="primary" id="fix">Fix it and land again</button>` : "";
  const dropIt = `<button id="drop-task">Drop this change</button>`;
  if (not.reason === "red") {
    const red = proof.steps.find((s) => s.exit !== 0);
    let log = "";
    if (red) {
      const l = await api(`${R(repo)}/proofs/${enc(proof.id)}/log?step=${enc(red.name)}&window=60`).catch(() => null);
      log = `<pre class="log">${esc((l ? l.lines.map((x) => (Array.isArray(x) ? x[1] : x)).join("\n") : red.output) || `(it printed nothing: exit ${red.exit})`)}</pre>`;
    }
    return `<div class="card landing red"><div class="bar" style="margin:0"><span class="dot red"></span><strong class="grow">Red: not landed</strong>${verdictPill("red")}</div>${stepRows}
      ${red ? `<p class="small" style="margin-top:var(--avp-space-3)">What <strong>${esc(red.name)}</strong> said:</p>${log}` : ""}
      <p class="small soft" style="margin-top:var(--avp-space-3)">The line did not move. Your change waits in its task: fix it and land again, it is proven again on the head.</p>${taskLink(repo, task)}
      <div class="bar" style="margin:0"><span class="grow"></span>${dropIt}${leave}${fix}</div></div>`;
  }
  if (not.reason === "approval") {
    return `<div class="card landing"><div class="bar" style="margin:0"><span class="dot wait"></span><strong class="grow">Waiting for a person's approval</strong>${proof ? verdictPill(proof.verdict) : ""}</div>${stepRows}
      ${proof ? "" : `<p class="small soft" style="margin-top:var(--avp-space-3)">Its proof runs now on the head of the line. Once green, an approval lands it.</p>`}
      <p style="margin-top:var(--avp-space-3)">${esc(not.guarded.join(", ") || "A path it touches")} ${not.guarded.length > 1 ? "are" : "is"} guarded: a change to ${not.guarded.length > 1 ? "them" : "it"} lands once an admin of the repo approves it. It lands then, proven as written.</p>
      ${cmd(`avp approve ${task}`)}${taskLink(repo, task)}<div class="bar" style="margin:0"><span class="grow"></span>${dropIt}${leave}</div></div>`;
  }
  if (not.reason === "conflict") {
    return `<div class="card landing red"><strong>The line changed the same lines meanwhile</strong>
      <p>${esc(not.conflicts.join(", "))} ${not.conflicts.length > 1 ? "were" : "was"} changed on the head since position ${ctx.head.position}, where you started. Drop this change and make it again on the new head.</p>
      ${taskLink(repo, task)}<div class="bar" style="margin:0"><span class="grow"></span>${dropIt}</div></div>`;
  }
  const why = { empty: "The change is empty: the files are as the head has them.", blocked: "Its proof waits for other tasks to land first.", refused: `A person refused it${not.word?.reason ? `: ${not.word.reason}` : ""}.`, already: "It has landed already." }[not.reason] || not.note || not.reason;
  return `<div class="card landing"><strong>Not landed</strong><p>${esc(why)}</p>${stepRows}${taskLink(repo, task)}<div class="bar" style="margin:0"><span class="grow"></span>${dropIt}${leave}${fix}</div></div>`;
}
