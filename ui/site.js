// AVP's website: the pages a person lives in (sign-in, home, organizations, repos), read from the
// same API the CLI and the agents use. One shell (site.html) for every page; this file routes by
// the address, repo.js adds a repo's pages. No library, no build step: the server serves ui/ as is.
"use strict";

const $ = (id) => document.getElementById(id);
const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
const enc = encodeURIComponent;
const ORIGIN = location.origin;

// Every request says it comes from this site (x-avp-live): the session cookie is taken for a POST
// only then, which a form on another site cannot send.
const LIVE = { "x-avp-live": "1" };

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

// A JSON answer, or an ApiError carrying the server's own sentence (it says what to do next). A 401
// on a page that needs a person sends them to sign in, back here after.
async function api(path, opts = {}) {
  const init = { cache: "no-store", method: opts.method || "GET", headers: { ...LIVE } };
  if (opts.body !== undefined) {
    init.headers["content-type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }
  const r = await fetch(path, init);
  const text = await r.text();
  let body = null;
  try { body = text ? JSON.parse(text) : null; } catch { body = text; }
  if (!r.ok) {
    if (r.status === 401 && !opts.quiet) toSignIn();
    throw new ApiError(r.status, (body && body.error) || (typeof body === "string" && body) || `${r.status} ${r.statusText}`);
  }
  return body;
}
// A route another task of AVP Cloud may not have landed on this server yet: null, not an error.
const maybe = (path) => api(path, { quiet: true }).catch((e) => (e.status === 404 || e.status === 405 ? null : Promise.reject(e)));

function toSignIn() {
  if (/^\/(signin|signup)/.test(location.pathname)) return;
  go(`/signin?next=${enc(location.pathname + location.search)}`, true);
}

// ---- formats ----

const LOCALE = navigator.language || "en-GB";
const clock = (ms) => new Date(ms).toLocaleTimeString(LOCALE, { hour: "2-digit", minute: "2-digit" });
function when(ms) {
  if (!ms) return "";
  const d = new Date(ms);
  return d.toDateString() === new Date().toDateString() ? clock(ms) : d.toLocaleString(LOCALE, { day: "numeric", month: "short", year: d.getFullYear() === new Date().getFullYear() ? undefined : "numeric", hour: "2-digit", minute: "2-digit" });
}
function ago(ms) {
  if (!ms) return "";
  const s = Math.max(0, (Date.now() - ms) / 1000);
  return s < 60 ? "just now" : s < 3600 ? `${Math.round(s / 60)} min ago` : s < 86400 ? `${Math.round(s / 3600)} h ago` : s < 30 * 86400 ? `${Math.round(s / 86400)} d ago` : when(ms);
}
// A span as a person says it: seconds, minutes, hours.
function dur(sec) {
  if (sec == null || !isFinite(sec)) return "";
  return sec < 1 ? `${Math.round(sec * 1000)} ms` : sec < 60 ? `${sec.toFixed(sec < 10 ? 1 : 0)} s` : sec < 3600 ? `${Math.floor(sec / 60)} min ${Math.round(sec % 60)} s` : `${(sec / 3600).toFixed(1)} h`;
}
const plural = (n, one, many = one + "s") => `${n.toLocaleString(LOCALE)} ${n === 1 ? one : many}`;
// A goal's first sentence, cut at a word.
const short = (g, n = 100) => { const s = String(g || "").split(/\.\s/)[0]; if (s.length <= n) return s; const c = s.slice(0, n - 1); return c.slice(0, Math.max(c.lastIndexOf(" "), n * 0.6)) + "…"; };

// Every coding agent by the name a person knows it by, whatever it calls itself.
const AGENT_NAMES = [
  [/claude/, "Claude Code"], [/codex/, "Codex"], [/cursor/, "Cursor"], [/gemini/, "Gemini CLI"], [/copilot/, "GitHub Copilot"],
  [/opencode/, "OpenCode"], [/windsurf|codeium/, "Windsurf"], [/^amp\b|ampcode/, "Amp"], [/aider/, "Aider"], [/qwen/, "Qwen Code"],
  [/kiro/, "Kiro"], [/^zed/, "Zed"], [/cline/, "Cline"], [/^roo|roo-?code/, "Roo Code"], [/goose/, "Goose"], [/junie/, "Junie"], [/continue/, "Continue"],
];
function agentName(id) {
  const a = String(id || "human").toLowerCase();
  if (a === "human") return "a person";
  if (a === "full suite" || a === "avp") return "AVP";
  if (a.startsWith("pr-")) return "PR " + a.slice(3);
  return AGENT_NAMES.find(([re]) => re.test(a))?.[1] || String(id);
}
const actorLine = (a) => !a ? "" : [a.person, a.agent && a.agent !== "human" ? `through ${agentName(a.agent)}` : ""].filter(Boolean).join(" ");
function verdictPill(v) {
  if (!v) return `<span class="pill">no proof</span>`;
  const cls = v === "green" ? "green" : v === "red" ? "red" : "";
  return `<span class="pill ${cls}">${esc(v)}</span>`;
}

// ---- the three states every page has ----

const loading = (what = "Loading") => `<div class="loading">${esc(what)}</div>`;
function empty(title, body = "", cmds = []) {
  return `<div class="state"><h2>${esc(title)}</h2>${body ? `<p>${body}</p>` : ""}${cmds.map(cmd).join("")}</div>`;
}
function failed(e, again = true) {
  const status = e && e.status;
  const title = status === 404 ? "Not found" : status === 403 ? "Not allowed" : "Something went wrong";
  return `<div class="state error"><h2>${title}</h2><p>${esc(e && e.message || e)}</p>${again ? `<button data-again>Try again</button>` : ""}</div>`;
}
// A command a person copies: shown whole, one click to copy.
function cmd(text) {
  return `<div class="cmd">${esc(text)}<button type="button" data-copy="${esc(text)}">Copy</button></div>`;
}
document.addEventListener("click", (ev) => {
  if (ev.target.closest("[data-again]")) return render();
  const b = ev.target.closest("[data-copy]");
  if (!b) return;
  navigator.clipboard?.writeText(b.dataset.copy).then(() => toast("Copied"), () => toast("Select it and copy"));
});
function toast(msg) {
  const t = document.createElement("div");
  t.className = "toast";
  t.textContent = msg;
  document.body.append(t);
  setTimeout(() => t.remove(), 1800);
}

// ---- who is here ----

let ME = null;
// Who the server takes this browser for: on a server without accounts, everyone (and no sign-in).
async function me(fresh = false) {
  if (ME && !fresh) return ME;
  const ways = await api("/auth", { quiet: true }).catch(() => ({ accounts: false }));
  if (!ways.accounts) {
    ME = { accounts: false, person: null, admin: true, github: false, roles: [] };
    return ME;
  }
  ME = await api("/me", { quiet: true }).catch(() => ({ accounts: true, person: null, github: !!ways.github, roles: [] }));
  ME.ways = ways;
  return ME;
}
// The server's own machine (127.0.0.1, no proxy) has every right without signing in, as the CLI there does.
const signedIn = () => ME && (!ME.accounts || ME.person || ME.admin);

function drawMe() {
  const el = $("me");
  if (!ME || !ME.accounts) {
    el.innerHTML = `<a href="/live" class="soft">Live</a>`;
    return;
  }
  el.innerHTML = ME.person
    ? `<a href="/live" class="soft hide-sm">Live</a><a href="/settings" class="who">${esc(ME.person)}</a><button class="link" id="signout">Sign out</button>`
    : `<a href="/signin">Sign in</a><a class="button primary" href="/signup">Sign up</a>`;
  $("signout")?.addEventListener("click", async () => {
    await fetch("/session", { method: "DELETE", headers: LIVE }).catch(() => {});
    ME = null;
    go("/signin");
  });
}

function crumbs(parts) {
  $("crumbs").innerHTML = parts.map(([label, href]) => href ? `<a href="${href}">${esc(label)}</a>` : `<span>${esc(label)}</span>`).join(`<span class="sep">/</span>`);
}

// ---- the router ----

const ROUTES = [];
// A page: a pattern on the path, its render; `open` pages show to someone signed out.
function page(re, fn, open = false) { ROUTES.push({ re, fn, open }); }

function go(href, replace = false) {
  if (replace) history.replaceState(null, "", href); else history.pushState(null, "", href);
  render();
}
// Links of this site change the page without reloading it; a modifier key keeps the browser's way.
document.addEventListener("click", (ev) => {
  const a = ev.target.closest("a[href]");
  if (!a || ev.defaultPrevented || ev.button !== 0 || ev.metaKey || ev.ctrlKey || ev.shiftKey || ev.altKey || a.target) return;
  const url = new URL(a.href, location.href);
  if (url.origin !== ORIGIN || !isPage(url.pathname)) return;
  ev.preventDefault();
  if (url.pathname === location.pathname && url.search === location.search && url.hash) { location.hash = url.hash; return; }
  go(url.pathname + url.search + url.hash);
});
addEventListener("popstate", () => render());
const isPage = (p) => ROUTES.some((r) => r.re.test(p));

let RENDER = 0;
// The current render: a page that finishes after the person moved on draws nothing.
const current = (n) => n === RENDER;

async function render() {
  const n = ++RENDER;
  const path = location.pathname.replace(/\/+$/, "") || "/";
  const route = ROUTES.find((r) => r.re.test(path));
  const el = $("page");
  el.className = "";
  el.removeAttribute("style");
  try {
    await Promise.all([me(), INSTALL_KNOWN]);
  } catch {}
  drawMe();
  if (!route) {
    crumbs([]);
    el.innerHTML = failed(new ApiError(404, `No page at ${path}.`), false);
    return;
  }
  if (!route.open && !signedIn()) return toSignIn();
  const params = path.match(route.re).slice(1).map(decodeURIComponent);
  el.innerHTML = loading();
  try {
    await route.fn(el, params, new URLSearchParams(location.search), n);
  } catch (e) {
    if (current(n)) el.innerHTML = failed(e);
  }
  if (current(n) && location.hash) document.getElementById(location.hash.slice(1))?.scrollIntoView({ block: "center" });
}

// ---- sign up, sign in, the device of `avp login` ----

const nextOf = (q) => { const n = q.get("next") || "/"; return n.startsWith("/") && !n.startsWith("//") ? n : "/"; };

page(/^\/signin$/, async (el, _, q) => {
  crumbs([["Sign in"]]);
  el.className = "narrow";
  const m = await me(true);
  if (!m.accounts) {
    el.innerHTML = empty("No sign-in here", "This server has no accounts: everything on it is open to whoever reaches it.", []) + `<p class="state"><a class="button primary" href="/">Go home</a></p>`;
    return;
  }
  if (m.person) return go(nextOf(q), true);
  const why = q.get("signin");
  el.innerHTML = `
    <h1>Sign in to AVP</h1>
    ${why ? `<div class="notice">${esc(why)}</div>` : ""}
    ${m.github ? `<a class="button wide" href="/auth/github?next=${enc(nextOf(q))}">Sign in with GitHub</a><div class="or">or</div>` : ""}
    <form id="pw">
      <label>Name or email <input name="name" autocomplete="username" required></label>
      <label>Password <input name="password" type="password" autocomplete="current-password" required></label>
      <p class="small" id="err" role="alert"></p>
      <button class="primary wide">Sign in</button>
    </form>
    <details style="margin-top:var(--avp-space-4)"><summary class="soft small">Sign in with a token</summary>
      <form id="tk" style="margin-top:var(--avp-space-3)">
        <label>Token <input name="token" autocomplete="off" required placeholder="avp_..."></label>
        <p class="small soft">One of yours from <code>avp token create</code>. It never stays in the browser: the server gives the page a session of its own.</p>
        <button class="wide">Sign in with this token</button>
      </form>
    </details>
    <p class="soft" style="margin-top:var(--avp-space-5)">New here? <a href="/signup${location.search}">Create an account</a></p>`;
  const done = async () => { await me(true); go(nextOf(q), true); };
  const submit = (form, body) => form.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const b = form.querySelector("button");
    b.disabled = true;
    try {
      await api("/session", { method: "POST", body: body(new FormData(form)), quiet: true });
      await done();
    } catch (e) {
      $("err").textContent = e.message;
      b.disabled = false;
    }
  });
  submit($("pw"), (f) => ({ name: f.get("name").trim(), password: f.get("password") }));
  submit($("tk"), (f) => ({ token: f.get("token").trim() }));
}, true);

page(/^\/signup$/, async (el, _, q) => {
  crumbs([["Sign up"]]);
  el.className = "narrow";
  const m = await me(true);
  if (m.person) return go(nextOf(q), true);
  el.innerHTML = `
    <h1>Create your AVP account</h1>
    <p class="soft">Version control for coding agents: tasks, proofs and one line, for every agent you run.</p>
    ${m.github ? `<a class="button wide" href="/auth/github?next=${enc(nextOf(q))}&signup=1">Sign up with GitHub</a><div class="or">or</div>` : ""}
    <form id="su">
      <label>Name <input name="name" autocomplete="username" required pattern="[A-Za-z0-9][A-Za-z0-9._-]*" title="Letters, digits, dots, dashes" placeholder="ada"></label>
      <label>Email <input name="email" type="email" autocomplete="email" required></label>
      <label>Password <input name="password" type="password" autocomplete="new-password" required minlength="8"></label>
      <p class="small" id="err" role="alert"></p>
      <button class="primary wide">Create account</button>
    </form>
    <p class="soft" style="margin-top:var(--avp-space-5)">Already have one? <a href="/signin${location.search}">Sign in</a></p>`;
  $("su").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    const b = ev.target.querySelector("button");
    b.disabled = true;
    try {
      await api("/signup", { method: "POST", body: { name: f.get("name").trim(), email: f.get("email").trim(), password: f.get("password") }, quiet: true });
      await me(true);
      go(nextOf(q) === "/" ? "/new" : nextOf(q), true);
    } catch (e) {
      $("err").textContent = e.status === 404 || e.status === 405 ? "This server takes no sign-ups: a server admin adds you with `avp user add <name>`." : e.message;
      b.disabled = false;
    }
  });
}, true);

// `avp login` shows a code and opens this page: the person signed in here says yes, the terminal
// gets a token of theirs.
page(/^\/login\/device$/, async (el, _, q) => {
  crumbs([["Sign in a device"]]);
  el.className = "narrow";
  const code = (q.get("code") || "").toUpperCase();
  const asks = code ? await maybe(`/login/device/peek?code=${enc(code)}`).catch(() => null) : null;
  el.innerHTML = `
    <h1>Sign in a terminal</h1>
    <p class="soft">Signed in as <strong>${esc(ME.person || "this server's person")}</strong>.${asks?.machine ? ` The machine <strong>${esc(asks.machine)}</strong> asks.` : ""} Check that the code below is the one your terminal shows, then approve: <code>avp</code> there gets a token of yours.</p>
    <form id="dv">
      <label>Code <input name="code" value="${esc(code)}" required autocomplete="off" placeholder="ABCD-EFGH" style="font-family:var(--avp-mono);font-size:var(--avp-size-lg);letter-spacing:0.12em;text-transform:uppercase"></label>
      <p class="small" id="err" role="alert"></p>
      <div class="row"><button class="primary" name="yes">Approve</button><button type="button" id="no">Deny</button></div>
    </form>`;
  const answer = async (approve) => {
    const c = new FormData($("dv")).get("code").trim().toUpperCase();
    if (!c) return;
    try {
      await api("/login/device/approve", { method: "POST", body: { code: c, deny: !approve }, quiet: true });
      el.innerHTML = approve
        ? empty("Your terminal is signed in", "Back to it: <code>avp whoami</code> says who it works as. Revoke it any time from your <a href=\"/settings\">tokens</a>.")
        : empty("Denied", "That terminal gets no token. Run <code>avp login</code> again to start over.");
    } catch (e) {
      $("err").textContent = e.status === 404 || e.status === 405 ? "This server does not sign terminals in from the browser yet: make a token in your settings and put it in AVP_TOKEN." : e.message;
    }
  };
  $("dv").addEventListener("submit", (ev) => { ev.preventDefault(); answer(true); });
  $("no").addEventListener("click", () => answer(false));
});

// ---- home ----

// The three commands that put a project on this server, the server filled in. A server that serves
// its own installer (`/install`) gives one that logs in to it; another, the release's.
let INSTALL = "curl -fsSL https://raw.githubusercontent.com/Mossab28/avp/main/install.sh | sh";
const INSTALL_KNOWN = fetch("/install", { method: "HEAD" }).then((r) => { if (r.ok) INSTALL = `curl -fsSL ${ORIGIN}/install | sh`; }).catch(() => {});
function setupCommands(repo) {
  return [
    INSTALL,
    `avp login ${ORIGIN}`,
    repo ? `cd ${repo} && avp up` : "cd your-project && avp up",
  ];
}

// Organizations, when this server has them; a server without is one organization of its own.
async function orgs() {
  const list = await maybe("/orgs");
  return Array.isArray(list) ? list : list?.orgs || null;
}
const repoOrg = (r) => r.org || (r.name.includes("/") ? r.name.split("/")[0] : null);
// A repo as it is said: `acme/shop` on AVP Cloud (its space, then its name), `shop` on a team server.
const fullName = (r) => (r.space ? `${r.space}/${r.name}` : r.name);
// In an address, each part encoded and the `/` between kept: `/r/acme/shop`, `/repos/acme/shop`.
const repoPath = (name) => name.split("/").map(enc).join("/");

page(/^\/$/, async (el, _, q, n) => {
  crumbs([]);
  const [repos, os, invites] = await Promise.all([api("/repos"), orgs().catch(() => null), maybe("/invites").catch(() => null)]);
  if (!current(n)) return;
  const heads = await Promise.all(repos.map((r) => api(`/repos/${repoPath(fullName(r))}/positions?limit=1`).then((p) => p[0]).catch(() => null)));
  if (!current(n)) return;
  const byRepo = new Map(repos.map((r, i) => [r.id, heads[i]]));
  const repoItem = (r) => {
    const h = byRepo.get(r.id);
    return `<a class="item" href="/r/${repoPath(fullName(r))}"><span class="main"><span class="title"><strong>${esc(fullName(r))}</strong></span>
      <span class="meta">${h ? `${esc(short(h.note, 80))}` : "no position yet"}</span></span>
      <span class="side">${h ? `position ${h.position}<br>${ago(h.landed_at)}` : ""}</span></a>`;
  };
  const sorted = [...repos].sort((a, b) => (byRepo.get(b.id)?.landed_at || b.created_at) - (byRepo.get(a.id)?.landed_at || a.created_at));
  const hello = ME.person ? `Hello, ${esc(ME.person)}` : "This server";
  if (!repos.length) {
    el.innerHTML = `<h1>${hello}</h1>` + empty("No repo yet",
      "A repo on AVP is a line of positions: each one a task an agent opened, wrote and landed, proven on the exact head first. Put a project here in three commands, from its folder:",
      setupCommands()) + invitesCard(invites) + (os?.length ? orgList(os) : "");
    answerInvites(el);
    return;
  }
  el.innerHTML = `
    <div class="bar"><h1 class="grow" style="margin:0">${hello}</h1><a class="button primary" href="/new">New repo</a></div>
    <div class="split">
      <section><h2>Repositories</h2><div class="list">${sorted.map(repoItem).join("")}</div></section>
      <aside class="stack">${invitesCard(invites)}${os ? orgList(os) : ""}
        <div class="card"><h3>Your agents, on this line</h3><p class="soft small">Every coding agent works through the same eleven verbs: Claude Code, Codex, Cursor, Gemini CLI, Copilot, OpenCode, Windsurf, Amp, Aider. <code>avp up</code> wires the ones it finds.</p></div>
      </aside>
    </div>`;
  answerInvites(el);
}, false);

// Invitations into an organization, answered here or with `avp org accept`.
function invitesCard(list) {
  if (!Array.isArray(list) || !list.length) return "";
  return `<div class="card"><h3>Invitations</h3><div class="list">${list.map((i) => `<div class="item" style="flex-wrap:wrap"><span class="main"><span class="title">${esc(i.org)}</span><span class="meta">${esc(i.role)} · from ${esc(i.by)}</span></span>
    <button class="primary" data-invite="${esc(i.id)}" data-accept="1">Join</button><button data-invite="${esc(i.id)}">Decline</button></div>`).join("")}</div></div>`;
}
function answerInvites(el) {
  el.querySelectorAll("[data-invite]").forEach((b) => b.addEventListener("click", async () => {
    try { await api(`/invites/${enc(b.dataset.invite)}`, { method: "POST", body: { accept: !!b.dataset.accept } }); await me(true); render(); } catch (e) { toast(e.message); }
  }));
}

function orgList(os) {
  return `<div class="card"><h3>Organizations</h3>${os.length ? `<div class="list">${os.map((o) => `<a class="item" href="/o/${enc(o.name)}"><span class="main"><span class="title">${esc(o.name)}</span><span class="meta">${esc(o.plan || "")}</span></span></a>`).join("")}</div>` : `<p class="soft small">None yet.</p>`}</div>`;
}

page(/^\/new$/, async (el) => {
  crumbs([["New repo"]]);
  el.className = "narrow";
  el.style.maxWidth = "640px";
  el.innerHTML = `
    <h1>Put a project on AVP</h1>
    <p class="soft">From the project's folder, a Git checkout or not. <code>avp up</code> creates the repo from its history, reads its CI into the checks every landing proves, and wires the coding agents it finds.</p>
    <h3>1. Install avp</h3>${cmd(setupCommands()[0])}
    <h3>2. Sign this terminal in</h3>${cmd(setupCommands()[1])}
    <p class="small soft">It opens <a href="/login/device">a page here</a> to approve the terminal.</p>
    <h3>3. Put the project on the line</h3>${cmd(setupCommands()[2])}
    <p class="soft small">Then tell your agent to work through AVP: it opens a task, writes, lands. The line shows up here and on <a href="/live">Live</a>.</p>
    <p><a href="/">Back home</a></p>`;
});

// ---- an organization ----

page(/^\/o\/([^/]+)$/, async (el, [name], q, n) => {
  crumbs([[name]]);
  const [org, usage] = await Promise.all([api(`/orgs/${enc(name)}`), api(`/orgs/${enc(name)}/usage`).catch(() => null)]);
  if (!current(n)) return;
  const admin = ME.admin || ["admin", "owner"].includes(org.role);
  const repos = org.repos || [];
  const mems = org.members || [];
  const invs = org.invites || [];
  const u = usage;
  el.innerHTML = `
    <div class="bar"><h1 class="grow" style="margin:0">${esc(org.name)}</h1><span class="pill solid">${esc(org.plan || "free")}</span>${org.role ? `<span class="pill">${esc(org.role)}</span>` : ""}<a class="button primary" href="/new">New repo</a></div>
    <div class="split">
      <section class="stack">
        <div><h2>Repositories</h2>${repos.length ? `<div class="list">${repos.map((r) => `<a class="item" href="/r/${repoPath(`${org.name}/${r}`)}"><span class="main"><span class="title">${esc(`${org.name}/${r}`)}</span></span></a>`).join("")}</div>` : empty("No repo yet", "From a project's folder, then bring it here:", [...setupCommands(), `avp org repo ${org.name} <repo>`])}</div>
        <div><h2>Members <span class="soft small">${plural(mems.length, "seat")}</span></h2><div class="list">${mems.map((m) => `<div class="item"><span class="main"><span class="title">${esc(m.person)}</span></span><span class="pill">${esc(m.role)}</span></div>`).join("")}</div></div>
        ${admin ? `<div><h2>Invitations</h2>${invs.length ? `<div class="list">${invs.map((i) => `<div class="item"><span class="main"><span class="title">${esc(i.person || i.email)}</span><span class="meta">${esc(i.role)} · by ${esc(i.by)} · ${ago(i.at)}</span></span></div>`).join("")}</div>` : `<p class="soft">None waiting.</p>`}
          <form id="inv" class="row" style="margin-top:var(--avp-space-3)"><input name="to" placeholder="a name or teammate@example.com" required><select name="role" style="width:120px"><option>member</option><option>admin</option><option>owner</option></select><button>Invite</button></form><p class="small" id="err" role="alert"></p></div>` : ""}
      </section>
      <aside class="stack">
        <div class="card"><h3>Proof minutes this month</h3>${u ? `
          <p style="font-size:var(--avp-size-xl);margin:0">${Math.round(u.minutes).toLocaleString(LOCALE)}${u.cap != null ? ` <span class="soft" style="font-size:var(--avp-size)">of ${Math.round(u.cap).toLocaleString(LOCALE)}</span>` : ""}</p>
          ${u.cap ? `<div class="meter" style="margin-top:var(--avp-space-2)"><i style="width:${Math.min(100, (100 * u.minutes) / u.cap).toFixed(1)}%"></i></div>` : ""}
          <p class="small soft" style="margin-top:var(--avp-space-2)">${Math.round(u.proof_minutes)} min of proofs, ${Math.round(u.full_minutes)} of full suites. ${u.over ? "<strong>Past the cap: new proofs wait for the month to end or a bigger plan.</strong> " : ""}${u.cap == null ? "No cap on this plan." : ""} Starts again ${when(Date.parse(u.resets_at))}.</p>` : `<p class="soft small">Nothing measured yet.</p>`}</div>
        <div class="card"><h3>Plan</h3><p>${esc(org.plan || "free")}</p>${ME.admin ? cmd(`avp org plan ${org.name} team`) : `<p class="small soft">A server admin changes it.</p>`}</div>
      </aside>
    </div>`;
  $("inv")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    try {
      await api(`/orgs/${enc(name)}/invites`, { method: "POST", body: { to: f.get("to").trim(), role: f.get("role") } });
      render();
    } catch (e) { $("err").textContent = e.message; }
  });
});

// ---- a person's own settings: their tokens ----

page(/^\/settings$/, async (el, _, q, n) => {
  crumbs([["Settings"]]);
  if (!ME.accounts) {
    el.innerHTML = empty("No accounts on this server", "Whoever reaches it may do everything: tokens and roles come with the first <code>avp user add</code>.");
    return;
  }
  const tokens = await api("/tokens");
  if (!current(n)) return;
  const live = tokens.filter((t) => !t.revoked_at);
  el.innerHTML = `
    <h1>${esc(ME.person)}</h1>
    <dl class="facts" style="margin-bottom:var(--avp-space-5)"><dt>Server admin</dt><dd>${ME.admin ? "yes" : "no"}</dd>
      <dt>Roles</dt><dd>${ME.roles?.length ? ME.roles.map((r) => `<a href="/r/${repoPath(r.repo)}">${esc(r.repo)}</a> <span class="pill">${esc(r.role)}</span>`).join(" ") : "none yet"}</dd></dl>
    <h2>Tokens</h2>
    <p class="soft">What a terminal, an agent or a machine of yours signs in with (<code>AVP_TOKEN</code>). Shown once when made, kept hashed.</p>
    <form id="mk" class="row" style="margin-bottom:var(--avp-space-4)"><input name="name" placeholder="laptop" required><select name="expires" style="width:140px"><option value="90d">90 days</option><option value="30d">30 days</option><option value="never">never</option></select><button class="primary">Create token</button></form>
    <div id="new"></div>
    ${live.length ? `<div class="list">${live.map((t) => `<div class="item"><span class="main"><span class="title">${esc(t.name)} <span class="faint small mono">${esc(t.id)}</span></span>
      <span class="meta">${esc(t.kind)} · made ${ago(t.created_at)}${t.used_at ? ` · used ${ago(t.used_at)}` : " · never used"}${t.expires_at ? ` · ends ${when(t.expires_at)}` : ""}</span></span>
      <button data-revoke="${esc(t.id)}">Revoke</button></div>`).join("")}</div>` : empty("No token", "Make one for each machine, so one lost is one revoked.")}`;
  $("mk").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    try {
      const t = await api("/tokens", { method: "POST", body: { name: f.get("name"), expires: f.get("expires") } });
      $("new").innerHTML = `<div class="notice"><strong>${esc(t.name)}</strong>: copy it now, it is not shown again.${cmd(`export AVP_TOKEN=${t.token}`)}</div>`;
      ev.target.reset();
    } catch (e) { toast(e.message); }
  });
  el.querySelectorAll("[data-revoke]").forEach((b) => b.addEventListener("click", async () => {
    if (!confirm("Revoke this token? What signs in with it stops at once.")) return;
    try { await api(`/tokens/${enc(b.dataset.revoke)}/revoke`, { method: "POST", body: {} }); render(); } catch (e) { toast(e.message); }
  }));
});

addEventListener("DOMContentLoaded", () => render());
