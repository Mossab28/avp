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
// Every page of the site, filled by page() below and in repo.js.
const ROUTES = [];

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

// The site speaks English: its dates and numbers too, whatever the browser says.
const LOCALE = "en-GB";
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
// Who made a position no task landed: a repo created on the web names its person in its note.
const madeBy = (p) => { const m = String(p.note || "").match(/^created (?:on the web by ([^,]+)|with a README|empty)/); return m ? (m[1] ? `created on the web by ${m[1]}` : "created on the web") : "imported"; };
const actorLine = (a) => !a ? "" : [a.person, a.agent && a.agent !== "human" ? `through ${agentName(a.agent)}` : ""].filter(Boolean).join(" ");

// ---- marks: icons, a proof's status, a position, an id, a person ----

// One small set of icons, drawn as strokes on a 24 grid (Lucide's way), inline: no icon font.
const ICONS = {
  code: '<path d="m16 18 6-6-6-6"/><path d="m8 6-6 6 6 6"/>',
  history: '<path d="M3 12a9 9 0 1 0 3-6.7L3 8"/><path d="M3 3v5h5"/><path d="M12 7v5l4 2"/>',
  tasks: '<path d="m3 17 2 2 4-4"/><path d="m3 7 2 2 4-4"/><path d="M13 6h8"/><path d="M13 12h8"/><path d="M13 18h8"/>',
  proofs: '<path d="M20 13c0 5-3.5 7.5-7.66 8.95a1 1 0 0 1-.67-.01C7.5 20.5 4 18 4 13V6a1 1 0 0 1 1-1c2 0 4.5-1.2 6.24-2.72a1.17 1.17 0 0 1 1.52 0C14.51 3.81 17 5 19 5a1 1 0 0 1 1 1z"/><path d="m9 12 2 2 4-4"/>',
  envs: '<path d="m12 2 10 5-10 5L2 7z"/><path d="m2 17 10 5 10-5"/><path d="m2 12 10 5 10-5"/>',
  activity: '<path d="M22 12h-4l-3 9L9 3l-3 9H2"/>',
  settings: '<path d="M4 21v-7"/><path d="M4 10V3"/><path d="M12 21v-9"/><path d="M12 8V3"/><path d="M20 21v-5"/><path d="M20 12V3"/><path d="M2 14h4"/><path d="M10 8h4"/><path d="M18 16h4"/>',
  folder: '<path d="M20 20a2 2 0 0 0 2-2V8a2 2 0 0 0-2-2h-7.9a2 2 0 0 1-1.69-.9L9.6 3.9A2 2 0 0 0 7.93 3H4a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2Z"/>',
  file: '<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4"/>',
  plug: '<path d="M12 22v-5"/><path d="M9 8V2"/><path d="M15 8V2"/><path d="M18 8v5a4 4 0 0 1-4 4h-4a4 4 0 0 1-4-4V8Z"/>',
  copy: '<rect x="8" y="8" width="14" height="14" rx="2"/><path d="M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2"/>',
  x: '<path d="M18 6 6 18"/><path d="m6 6 12 12"/>',
  down: '<path d="m6 9 6 6 6-6"/>',
  right: '<path d="m9 18 6-6-6-6"/>',
  plus: '<path d="M5 12h14"/><path d="M12 5v14"/>',
  users: '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  key: '<circle cx="7.5" cy="15.5" r="5.5"/><path d="m21 2-9.6 9.6"/><path d="m15.5 7.5 3 3L22 7l-3-3"/>',
  lock: '<rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/>',
  alert: '<path d="m21.73 18-8-14a2 2 0 0 0-3.48 0l-8 14A2 2 0 0 0 4 21h16a2 2 0 0 0 1.73-3"/><path d="M12 9v4"/><path d="M12 17h.01"/>',
  log: '<path d="M15 12h-5"/><path d="M15 8h-5"/><path d="M19 17V5a2 2 0 0 0-2-2H4"/><path d="M8 21h12a2 2 0 0 0 2-2v-1a1 1 0 0 0-1-1H11a1 1 0 0 0-1 1v1a2 2 0 1 1-4 0V5a2 2 0 1 0-4 0v2a1 1 0 0 0 1 1h3"/>',
  user: '<circle cx="12" cy="8" r="5"/><path d="M20 21a8 8 0 0 0-16 0"/>',
  gauge: '<path d="m12 14 4-4"/><path d="M3.34 19a10 10 0 1 1 17.32 0"/>',
  out: '<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><path d="m16 17 5-5-5-5"/><path d="M21 12H9"/>',
  repo: '<path d="M4 19.5v-15A2.5 2.5 0 0 1 6.5 2H19a1 1 0 0 1 1 1v18a1 1 0 0 1-1 1H6.5a1 1 0 0 1 0-5H20"/>',
  land: '<circle cx="12" cy="12" r="3"/><path d="M3 12h6"/><path d="M15 12h6"/>',
  up: '<path d="M12 19V5"/><path d="m5 12 7-7 7 7"/>',
  terminal: '<path d="m4 17 6-6-6-6"/><path d="M12 19h8"/>',
  org: '<path d="M6 22V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2v18Z"/><path d="M6 12H4a2 2 0 0 0-2 2v6a2 2 0 0 0 2 2h2"/><path d="M18 9h2a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2h-2"/><path d="M10 6h4"/><path d="M10 10h4"/><path d="M10 14h4"/><path d="M10 18h4"/>',
};
const icon = (name, cls = "") => `<svg class="i ${cls}" viewBox="0 0 24 24" aria-hidden="true">${ICONS[name] || ""}</svg>`;

// A proof's verdict as a badge: green, red, running; a dot inside says it at a glance.
function verdictPill(v) {
  if (!v) return `<span class="pill">no proof</span>`;
  if (v === "none") return `<span class="pill">no checks</span>`;
  const cls = v === "green" ? "green" : v === "red" ? "red" : v === "running" || v === "pending" ? "run" : "";
  return `<span class="pill ${cls}"><span class="dot ${cls === "run" ? "" : cls}"></span>${esc(v)}</span>`;
}
// A green proof that ran no step proved nothing: it reads as "no checks", not as green.
const proofPill = (v, steps) => (v === "green" && steps && !steps.length ? verdictPill("none") : verdictPill(v));
const noChecks = (v, steps) => v === "none" || (v === "green" && !!steps && !steps.length);
const dateOf = (ms) => new Date(ms).toLocaleDateString(LOCALE, { day: "numeric", month: "short", year: "numeric" });
const verdictDot = (v) => `<span class="dot ${v === "green" ? "green" : v === "red" ? "red" : v === "running" ? "run" : v ? "" : "wait"}" title="${esc(v || "no proof")}"></span>`;
// A position as a chip (#12), a link to it when the repo is known.
const chip = (pos, repo) => repo ? `<a class="chip" href="/r/${repoPath(repo)}/p/${pos}">#${pos}</a>` : `<span class="chip">#${pos}</span>`;
// A raw id, muted: there to copy, not to read.
const idChip = (id) => `<span class="idchip" title="${esc(id)}">${esc(id)}</span>`;
// A person (or an agent) as their initials on a disc, its tone fixed by the name.
function avatar(name, size = "") {
  const n = String(name || "?");
  const parts = n.replace(/[^A-Za-z0-9 ._-]/g, "").split(/[ ._-]+/).filter(Boolean);
  const ini = (parts.length > 1 ? parts[0][0] + parts[1][0] : n.slice(0, 2)).toUpperCase();
  let h = 0;
  for (const c of n) h = (h * 31 + c.charCodeAt(0)) >>> 0;
  return `<span class="avatar t${(h % 4) + 1} ${size}" aria-hidden="true">${esc(ini)}</span>`;
}
const timeAgo = (ms) => ms ? `<time datetime="${new Date(ms).toISOString()}" title="${esc(when(ms))}">${esc(ago(ms))}</time>` : "";

// ---- the three states every page has ----

const loading = (what = "Loading") => `<div class="loading">${esc(what)}</div>`;
// Empty: one short line, then one thing to do (a link or commands to copy).
function empty(title, body = "", cmds = [], opts = {}) {
  const action = opts.href ? `<p><a class="button primary" href="${opts.href}">${esc(opts.action)}</a></p>` : "";
  return `<div class="state${opts.boxed ? " boxed" : ""}">${opts.icon ? `<span class="glyph">${icon(opts.icon)}</span>` : ""}<h2>${esc(title)}</h2>${body ? `<p>${body}</p>` : ""}${action}${cmds.map(cmd).join("")}</div>`;
}
function failed(e, again = true) {
  const status = e && e.status;
  const title = status === 404 ? "Not found" : status === 403 ? "Not allowed" : "Something went wrong";
  return `<div class="state error"><h2>${title}</h2><p>${esc(e && e.message || e)}</p>${again ? `<button data-again>Try again</button>` : ""}</div>`;
}
// A command a person copies: shown whole, one click to copy.
function cmd(text) {
  return `<div class="cmd${String(text).includes("\n") ? " block" : ""}">${esc(text)}<button type="button" data-copy="${esc(text)}">Copy</button></div>`;
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
    el.innerHTML = "";
    return;
  }
  // The person's menu: their account, usage, sign out. A <details>, closed by any press outside it.
  el.innerHTML = ME.person
    ? `<details class="menu"><summary class="me-btn" aria-label="Your menu">${avatar(ME.person, "lg")}</summary><div class="menu-pop">
        <div class="head">Signed in as<strong>${esc(ME.person)}</strong></div><hr>
        <a href="/">${icon("repo")}Your repositories</a><a href="/new">${icon("plus")}New repo</a>
        <a href="/settings">${icon("user")}Account and tokens</a>${ME.cloud !== false ? `<a href="/usage">${icon("gauge")}Usage</a>` : ""}<hr>
        <button type="button" id="signout">${icon("out")}Sign out</button></div></details>`
    : `<a href="/signin">Sign in</a><a class="button primary" href="/signup">Start free</a>`;
  $("signout")?.addEventListener("click", async () => {
    await fetch("/session", { method: "DELETE", headers: LIVE }).catch(() => {});
    ME = null;
    go("/signin");
  });
}
// A press outside an open menu, or Escape, closes it; following one of its links does too.
document.addEventListener("click", (ev) => {
  document.querySelectorAll("details.menu[open]").forEach((d) => { if (!d.contains(ev.target) || ev.target.closest(".menu-pop a")) d.open = false; });
});
document.addEventListener("keydown", (ev) => {
  if (ev.key !== "Escape") return;
  document.querySelectorAll("details.menu[open]").forEach((d) => { d.open = false; });
  closeSheet();
});

// Where the page is, in the bar: `owner / repo` with the repo switcher, or a page's name.
function crumbs(parts) {
  $("crumbs").innerHTML = parts.map(([label, href], i) => {
    const last = i === parts.length - 1;
    return href ? `<a href="${href}" class="${last ? "here" : ""}">${esc(label)}</a>` : `<span class="${last ? "here" : ""}">${esc(label)}</span>`;
  }).join(`<span class="sep">/</span>`);
}
let REPOS = null;
// The repos this person reaches, read once a visit: the switcher and the Connect page list them.
const reposKnown = () => (REPOS ||= api("/repos", { quiet: true }).then((rs) => rs.map(fullName).sort()).catch(() => { REPOS = null; return []; }));
function repoCrumbs(repo, extra = []) {
  const i = repo.lastIndexOf("/");
  const owner = i < 0 ? null : repo.slice(0, i), name = i < 0 ? repo : repo.slice(i + 1);
  const ownerHref = owner && owner !== ME?.person ? `/o/${enc(owner)}` : "/";
  $("crumbs").innerHTML = (owner ? `<a href="${ownerHref}" class="owner">${esc(owner)}</a><span class="sep owner">/</span>` : "")
    + `<a href="/r/${repoPath(repo)}" class="${extra.length ? "" : "here"}">${esc(name)}</a>`
    + `<details class="menu switch"><summary aria-label="Switch repository">${icon("down")}</summary><div class="menu-pop left" id="switch"><div class="head">Switch repository</div></div></details>`
    + extra.map(([label, href], k) => `<span class="sep">/</span>${href ? `<a href="${href}">${esc(label)}</a>` : `<span class="${k === extra.length - 1 ? "here" : ""}">${esc(label)}</span>`}`).join("");
  $("crumbs").querySelector(".switch").addEventListener("toggle", async (ev) => {
    if (!ev.target.open) return;
    const list = await reposKnown();
    $("switch").innerHTML = `<div class="head">Switch repository</div>${list.map((r) => `<a href="/r/${repoPath(r)}" class="${r === repo ? "on" : ""}">${icon("repo")}${esc(r)}</a>`).join("")}<hr><a href="/new">${icon("plus")}New repo</a>`;
  });
}

// The bar's tools: connecting an agent, to this repo or to one of yours.
function drawTools(repo) {
  $("tools").innerHTML = signedIn() && location.pathname !== "/connect" ? `<button type="button" class="sm" data-connect="${esc(repo || "")}">${icon("plug")}<span class="label-l">Connect agent</span></button>` : "";
}

// ---- connecting an agent: a page of its own, one tile per agent, the button that opens it ----

// What each agent reads, where `avp up` writes it (src/up.rs): the same `avp mcp` server, told
// this server and this repo. Aider has no MCP: it runs avp in its shell, from the rule avp up writes.
// The agents' own marks (Simple Icons paths; Amp and Aider their app icons), drawn on one dark tile.
const AGENT_MARKS = {
  claude: { color: "#D97757", d: "m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z" },
  codex: { color: "#ffffff", d: "M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" },
  cursor: { color: "#ffffff", d: "M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23" },
  gemini: { color: "#8E75FF", d: "M11.04 19.32Q12 21.51 12 24q0-2.49.93-4.68.96-2.19 2.58-3.81t3.81-2.55Q21.51 12 24 12q-2.49 0-4.68-.93a12.3 12.3 0 0 1-3.81-2.58 12.3 12.3 0 0 1-2.58-3.81Q12 2.49 12 0q0 2.49-.96 4.68-.93 2.19-2.55 3.81a12.3 12.3 0 0 1-3.81 2.58Q2.49 12 0 12q2.49 0 4.68.96 2.19.93 3.81 2.55t2.55 3.81" },
  copilot: { color: "#ffffff", d: "M23.922 16.997C23.061 18.492 18.063 22.02 12 22.02 5.937 22.02.939 18.492.078 16.997A.641.641 0 0 1 0 16.741v-2.869a.883.883 0 0 1 .053-.22c.372-.935 1.347-2.292 2.605-2.656.167-.429.414-1.055.644-1.517a10.098 10.098 0 0 1-.052-1.086c0-1.331.282-2.499 1.132-3.368.397-.406.89-.717 1.474-.952C7.255 2.937 9.248 1.98 11.978 1.98c2.731 0 4.767.957 6.166 2.093.584.235 1.077.546 1.474.952.85.869 1.132 2.037 1.132 3.368 0 .368-.014.733-.052 1.086.23.462.477 1.088.644 1.517 1.258.364 2.233 1.721 2.605 2.656a.841.841 0 0 1 .053.22v2.869a.641.641 0 0 1-.078.256Zm-11.75-5.992h-.344a4.359 4.359 0 0 1-.355.508c-.77.947-1.918 1.492-3.508 1.492-1.725 0-2.989-.359-3.782-1.259a2.137 2.137 0 0 1-.085-.104L4 11.746v6.585c1.435.779 4.514 2.179 8 2.179 3.486 0 6.565-1.4 8-2.179v-6.585l-.098-.104s-.033.045-.085.104c-.793.9-2.057 1.259-3.782 1.259-1.59 0-2.738-.545-3.508-1.492a4.359 4.359 0 0 1-.355-.508Zm2.328 3.25c.549 0 1 .451 1 1v2c0 .549-.451 1-1 1-.549 0-1-.451-1-1v-2c0-.549.451-1 1-1Zm-5 0c.549 0 1 .451 1 1v2c0 .549-.451 1-1 1-.549 0-1-.451-1-1v-2c0-.549.451-1 1-1Zm3.313-6.185c.136 1.057.403 1.913.878 2.497.442.544 1.134.938 2.344.938 1.573 0 2.292-.337 2.657-.751.384-.435.558-1.15.558-2.361 0-1.14-.243-1.847-.705-2.319-.477-.488-1.319-.862-2.824-1.025-1.487-.161-2.192.138-2.533.529-.269.307-.437.808-.438 1.578v.021c0 .265.021.562.063.893Zm-1.626 0c.042-.331.063-.628.063-.894v-.02c-.001-.77-.169-1.271-.438-1.578-.341-.391-1.046-.69-2.533-.529-1.505.163-2.347.537-2.824 1.025-.462.472-.705 1.179-.705 2.319 0 1.211.175 1.926.558 2.361.365.414 1.084.751 2.657.751 1.21 0 1.902-.394 2.344-.938.475-.584.742-1.44.878-2.497Z" },
  opencode: { color: "#ffffff", d: "M22 24H2V0h20zM17 4.8H7v14.4h10z" },
  windsurf: { color: "#5CE1E6", d: "M23.55 5.067c-1.2038-.002-2.1806.973-2.1806 2.1765v4.8676c0 .972-.8035 1.7594-1.7597 1.7594-.568 0-1.1352-.286-1.4718-.7659l-4.9713-7.1003c-.4125-.5896-1.0837-.941-1.8103-.941-1.1334 0-2.1533.9635-2.1533 2.153v4.8957c0 .972-.7969 1.7594-1.7596 1.7594-.57 0-1.1363-.286-1.4728-.7658L.4076 5.1598C.2822 4.9798 0 5.0688 0 5.2882v4.2452c0 .2147.0656.4228.1884.599l5.4748 7.8183c.3234.462.8006.8052 1.3509.9298 1.3771.313 2.6446-.747 2.6446-2.0977v-4.893c0-.972.7875-1.7593 1.7596-1.7593h.003a1.798 1.798 0 0 1 1.4718.7658l4.9723 7.0994c.4135.5905 1.05.941 1.8093.941 1.1587 0 2.1515-.9645 2.1515-2.153v-4.8948c0-.972.7875-1.7594 1.7596-1.7594h.194a.22.22 0 0 0 .2204-.2202v-4.622a.22.22 0 0 0-.2203-.2203Z" },
  amp: { img: "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAYAAACqaXHeAAAAAXNSR0IArs4c6QAAAERlWElmTU0AKgAAAAgAAYdpAAQAAAABAAAAGgAAAAAAA6ABAAMAAAABAAEAAKACAAQAAAABAAAAQKADAAQAAAABAAAAQAAAAABGUUKwAAAREklEQVR4AeVaCXCV1RU+7+VlJxsJSUhCAEEQEEWsZXVqXVC0tMq4oGCnYmm1dkCnaq3VurVaitOxVanTcazL1Erbqa0MbdGKO21dEEUQAdlD9oSQPXkv6fede+///pe8EHCsCXrh/Xc799zznXPuuef/IXDizInd8gUuoe7uLzR+gQK+wOYH9M+fBwScQQPiNd0QazdoDR/6vDiA4go4dFGcij06HFWFHTumPSDgA9wDchRoP61jTAHGbAZ3PLPGQduLzA0Y3z+mgmCMwYGVAdwbc7icDmL6MR1LYcaODQ8Ayt4QaMHoaLd0oxftOz04mngzpBncHgCpCcoJ7yCbWic9nFEqeoVbwbZHEreBW4DsBmcJdBuQDjicHoJGEUVbUfn9gP1tVaVdoIjtI9SljSiDwdKKFd5IRYVYnRizQXZDZ5FpZdr+8Z68omSBwZkIqQvTMJBUz7a2AR5/1NXtnOcMBrOZ4zKL0A882uYkGZggOuiCoDnLgEqJEea1b5oGmAOvIBSpgolVBlbZKS8eoM8hA91MkmbwBUEjmwQsUBUYDwfIDgOKIYzGfguK6nA8uEjbPoVw3hUwG1weQHkhlFrfWpiOb1C4AxCt3bjzGqclYlbLK34LXhVB5K5BvlSAVoPjQSAKFw8GOy06YJrOK9hTufEgUENiF6jV3WIH3vSdF1luum5weQAlo5R69g1I23UyW81EY4OlwnxUU+ojWKiq0drwJS+nGlUg1gw+BRio+qSwBqpxezPogBoIittHTYSaP3AsiPUgC6DWcc5RCyyY4NzgC4IUziBnyxTbp0WpEBZHojUerk+FEKOqx7VZY42O6WqjGDYHsQcYkSm0NaB02TbH2CZFF9AGYUrX1zFLh8pXaHHMaiDBMJjwc+BRewBtEIlE8OtSBtwhGAxKQkKCJKCOFghl08xgkBvDfnQ7jqEbVL8EgC6KjiHQBDHR1d2lMgaUJwhBHgYN5wIJhg8W+faHT4B3CPuHEhPAOAG8sDe3wXgAgFGZPRx404U8egvgeSQFZO0dHZKclCTHjy6VUaXFkpUxRDrDYamqrpWde8rkQGU1lAFhQiFVRlZmunJuam5ROq5NT0tRxRxqaiYkycnO0LqppVU6OsPgmS5JiYnS1t4urfiFACgzM0P5Nbe0SFNzq2RnZcjYUSUyoni4ZKSnqVxlFVWyc/c+qW84JElJiaoQPTDW6qoDpwmH90g9gFbib85XZsiVl3xNJp0wRsGHQglq1faOTqmuqZc33twojz79rHywdYfMOm2KPHDPjQrqmpt+KpswtmDhebJ0yeXy0Y7dctWyO+SE40fLQ/feAmt1y7U3/0z27DsgD933IxkzskR++chT8tSf18jkCWPlYYzRw2677yEZA+AXzztHRsMAqakpqvAIZGtrbRcq4Z/rXpc/rX5emqAsKpLu5IKiuRUsehiUpu83BhA43evHy66WBReeJynJSXLwUKO8teEDOYANKVhpyXA5fsxIuezCOTJ72hRZdtsKyc/NkaKCYSpU/cFDsLLAc0ZIft5QeX/zdmlsbJbSogIpKhwmVTV1UlNbLzlZmQBWJFmZQ7TPozY8P09pmlva5MbvfVMmQvnhcES2bd8tu/bsh4I7JXdotowbO0qVcs23LpPpp54sd65YCY+skkQogcfAFHsWbI8q6DcGRCLdcvsNi2XRxRdIGAI994+XZeVjq2Tbx3ukHZuTZWpKspw8abxcf81CmTntFLn31u/Le5u3CS1TVV0HhTXBGklSPDxfuhA79pWVS3t7p4woKtT4Qe+prWuQUyafIGmwKo9D2YFqPaPFhfkAEICigzJh3HGyZevH8sDKp+S/GzZJa1u7xpMQ4kXBsFxZMH+uLLpsnkw5cbzc/cPrYIjl0tLapnsY73eK8DSgNyV6nOj94zmce+ZMWP5cDTpP/uE5ue6W+2Tzjl0STA5JWkaapOLXBeH+/e4muXrZnfL8S+tl1IgiOferM3VNeWWNtEKINJx9Cskgt3d/hQo+opgKCEhFVQ3OdouUFOVrjGlsbJKqujoFThoWuvPGTVvl20vvkBfXvyXdCIhpiDHpiBnJQ1Kl5tAhWfHw47Li149JB47kSRPHyeUXzkUs6dA9GZB7/iIY4w2imu5ZkzgNlr1qwdc1qL3z3hZZ/uDvJIQAk5yaDJEY1c1aWig1PVVa4BH33P9btXoQYwS370ClxoFsBLKc7EwEw4jsLavQQFUE67KQJoy19AjGlZrag3KwoUmPHr2GpmmGV9z1i0eksv6gpA1Jw/ZBjT+MH5SDgXcIjs4zf10rL7/+lvKde9ZsGYpjxSNDb+SPR9prwxtxYcSzvWjUnjR+LNxutDJ4ctVqaYY2EzTwxVkDRsmIDzsRyNa99qYkQlEs+wCWG+bn5UgGBG+D2+4vr5LMIenwiKHqJcYjgojqhWp1ek1LWxviTbLGAMagl8Bz45btkpJibhF6Eo3Eq1UtS0XQKFD6s39fp/IXIn6MO24kjlvYsz6t7nkC1kCN8TUQ7ozIZAScVGxYU3dQNry/VQFS232t4Rzv740ffETsGjP2lVWqhYoK4N5QEINfRVWt5ObkyFB4RBjXKD2Cc8UIiORBj+jElZgJ92aAo9Ve/c+7+paoFlcQyCFA7MA7UAnwhO279koD4k5iYghxp0CV0YVY5mi8GmPwgL7/FBbkaQCphQLqEfmZrPRNbWYYFRtwxxMJrV2O880AVFoC904ISQ1cuO5ggxQW5Eo6jg0DWRk9AjlFPjyCYPfuL9fkJy83W5XQDs/bd6BC91fQ4N1tPcAoAW5NZWCMHkGeXMOjSS8iT+Mx5gho346F1Jpqr9gH+EhiiG5MzYExp/URS9ezx0wvIz1dr8cGWJvek4BkxkR8BDy4d1MTAh7ONhOjurpqqUQilQdLM8HpRCygR0BivUYJgFedyzx5i1CjVCpvINYsvOp4BEiXBL7kTWUcQkClxen68UqfMYAKqKtvoCElG66anpaqLkc2h/txn4njj1PPqcH11gAB6N4EzEL37kZQYu7AgFcNBdUhTxiOnIF78NpiDCCyEr0mA7gBQlKQn2uUwP0hlLoxaj3T2JN6oTcwmxyJWygTNwRvg937ysEraOghnOf+tt3nLUD32bJtp1qkAMkLrxXe3VRIX78wpBgGS35lxlRs1IVEqRqA2hUY3ZvW0YAHS5UCHPcoJw3S2xIkRbQaz24NcgJNsOwVSLrTp09V4ECjiqQMDgz5cj/mLIz45505S3lx/w+34cpGXHK0/lqvwb7smZgUkrc3btEozmCy+PJ5OE+J2CRCG/T68Uy2IXIvvuIbeE8oUoF4bunSQ5HvM+DxvWEPcoBQCpOiYcqGHgGmMhIxgslONa7ARsSQZOw5orhAbxBenXPPmilTkShxDyoEf70fGdErmEvMPO1kueDsWUqz5oXXpAJZJou7+vw1ldanB/BtrQrp6ePPrFYGM750EjLCJXo3t7S0A1gEUb5LNd7a1qH9JQsvkiWLLsLLSaeuUWvD1QryGPDSkBDxCqz0Ah6FoUKAXErdFQirMXjyOBTC7WmxhoZGffdYfvtSORHHqwU5AW8JxgNan25PT5t56mRZfttSycDtwUz1saf/pqmw3+o924f9JpiMK/CJVWuQ5o6T+fPOxovQ+cgLRsnv/7JWNn2IfL6pFec7EXdtqVyK+TlnTFdN8+zRFdW6cJZiujfoeJtUVtUJo3s2EhR6B/ME7sN3Aro1vYYpNxOn3Jxs9YqVjz4jc+ecLrOnnyJPPHg3Xnb+Ja+sf0eDJ48E48ecM6bJ/PPPlKE5WTp+850PqPVTkFrTX/sqfd4CXEBXC2P5zXf9SgPiFRefL7O+PEWmTZ2sZ5UBiwGO7s1X0A8/2inr33xPFl16gXqBCWbmJYg3Q0PjLqlFwJs0YYzGihZYmueUR4TXZBKO3X4cCbqmS5zCsO7GLdvwlveG3H/PD2T2jFPlhu8ulO9cOV+PChXApIrvEB04Ym+/u1l+8vPfyAbkIinwIir1cKXffxtk5tcBi9wOpqvXvopX0bNl6skT9NpidG7GuSPwV954W/747FopgrX5csT38nIkPCEEtl17yuRJHCW+EkciYXXxVaDlW2VFVbXe1WtfXI91SZpEJSDPp0ekkA9uIt4kFbV1smTZXXIOvGwOgtw4vH0y0tNIzBt27Nwra9etlzUvvC5NeIdhas5bob8SSC0t7J+KXMCsA8lFBK6difPMDxe8xviayvueH0uS+I4Abh2wLD75CN2PEZh9WpJZWgpeiiKwVAfiBt8VUtHnh5G21lZuoqkug+W1V10it15/tX47uGTxTdIG/gx0bfA67jEkLU3SwR9Lkfh0qAxhBGIqLYC8Q9My0PWnhFB/GvLmoekkMOfm7dioEq5MgQIIlkG4birOuCupAKoFwoFcEpHM8EdrUeAQEqzEjJD27ZCk4z2BiuDfMLzEBMWgxgy+TSbwcxcy9zQon+A6EfzqcVuQP9ckQ5HJqGl0yqWDOotBQ0XKXqXfDyK9VlBMgA5QHl/RPb2NuCmKGUTD9r0xf5/TRmAjKj5SQIF8C2ThR5fOcKf53kf+SoQ3TRyTbnyC5FLmqeSg4Fl7bbb41so6fjlsEIy/pG9mUfqeNH31Oa52h1K52tzndG1Gdpb9TIsBgPNwPKMATnCMNYrqRNFrS5XigdZxQxfvedhrMN6C/8eYXz28p/lJjGeZefxuvF7zDZM4vOJrs+nr9mr3nPd42EYguaTAv77n/Kfa9zI4Wh2+q33sQB/gp2z38TIxFJS8rCy1bH1jI857RGkVjH24IEcB6Ri0OL/7m3F/28yRLl75BEcgHptPMKafqyEcvFZdGSjY5r8shMNdUsbXaLR5DfMmiXoANMA1JEZR63Gyi33tmco2dci1dUXs48hvgdh1n6jnRWcb9CgpgXSzj1oh4GrkeQ8FcUso0h5b+YCbdUSHBZYHNRX1BLCNaq4HI9P9TBXALWOUQDBOGUQP5NolHftUkD7ZtgUDalA+qDfl4SYNeEvhBg9b4wgou8MSfdqT0R3Z4j96sZjR6ByGNPTrZMzD0eh550oeH1Ko1tjCDIJpf9bnks/cA7gpla56p8VhcmNnM261garn/R3rCwrToFaW1ACDoYLXcTPc33NgFOCTSq1ES3vFtgFCIXtTPVDZrnfeuR5aheHZ0MW9lci52DJgCnBpsUrq8wgVnjJCKcY3PA3ESM45rmVNb6IivRF6ltc3NDGLfZ2BuwZVfPsgAnoBaxQT2PDP5KoEHTJ68elCSXH4ldaS6EVAVuRhnzpl2Fqq2GpAMkFnff5vMCOsFVoBYoxxwYG1StEugPixuP9N5pTAOff/C11c0bFYzDE9//9oiJn4TDu0PorDbPaO9nSakvLlh8PRKUOqTwzGHfeRxGkO6BEw8tBGVni/eZ2wFpTxFXoAbAs6PTWgMUs4YAk5YAZN7e87nr56cHiAT6CjaTqcHuCjWWxpB+wW6CUr0cRzYYvSXJegQd+GBcNC57mQE2boaJ4D5gFGVisxEZm/CkRHMWZnFbCSMOOzg7E0pMUIB+28R9iPNgYkFVaZKCgM590EONgBpnLqBTYHoBJsgOQaa2c2VRMu0hsNEbtRguqAa9kwHV0S7zEgR4DurFehXyLKruABlJIDuJPdttA3KjC1IdajwbXk5WrTVO6qFG3FfwyIAqxkKrTaGmAVjg8A0RiIpEDLdLAUDYs2BrR2LEgqkH/VBeIDd6MDdg3SMoRNuTVt9SuBeA0G06C0JPQXN8+xmLZ1fTvuXxKvPXAeQLl5FKgEBQzB7XmnsdV67PcE7kfBOT8J+DmjH4n1yep/mi5bUVF6NgkAAAAASUVORK5CYII=" },
  aider: { img: "data:image/png;base64,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" },
};
// An agent's mark on its tile: the brand's own shape in its color, or its app icon.
const mark = (id, size = "") => {
  const m = AGENT_MARKS[id];
  if (!m) return "";
  return `<span class="mark ${size}" aria-hidden="true">${m.img ? `<img src="${m.img}" alt="">` : `<svg viewBox="0 0 24 24" style="--brand:${m.color}"><path d="${m.d}"/></svg>`}</span>`;
};
const json = (x) => JSON.stringify(x, null, 2);
const mcpEnv = (repo) => ({ AVP_SERVER: ORIGIN, AVP_REPO: repo || "owner/repo" });
const mcpPlain = (repo) => ({ command: "avp", args: ["mcp"], env: mcpEnv(repo) });
const b64 = (s) => btoa(unescape(encodeURIComponent(s)));
// `open`: a link the agent's app takes (Cursor and VS Code install an MCP server from one), or the
// command that starts it in a terminal; `by`: the same config by hand.
const CONNECT = [
  { id: "claude", name: "Claude Code", kind: "Terminal", run: "claude", by: (r) => ({ how: "Or by hand, from the project's folder:", text: `claude mcp add-json avp '${JSON.stringify({ type: "stdio", ...mcpPlain(r) })}'` }) },
  { id: "codex", name: "Codex", kind: "Terminal", run: "codex", by: (r) => ({ file: ".codex/config.toml", text: `[mcp_servers.avp]\ncommand = "avp"\nargs = ["mcp"]\nenv = { AVP_SERVER = "${ORIGIN}", AVP_REPO = "${r || "owner/repo"}" }`, note: "Codex loads it once it trusts the folder." }) },
  { id: "cursor", name: "Cursor", kind: "Editor", app: (r) => ({ label: "Open in Cursor", href: `cursor://anysphere.cursor-deeplink/mcp/install?name=avp&config=${enc(b64(JSON.stringify(mcpPlain(r))))}` }), by: (r) => ({ file: ".cursor/mcp.json", text: json({ mcpServers: { avp: { type: "stdio", ...mcpPlain(r) } } }) }) },
  { id: "copilot", name: "Copilot", kind: "VS Code", app: (r) => ({ label: "Open in VS Code", href: `vscode:mcp/install?${enc(JSON.stringify({ name: "avp", type: "stdio", ...mcpPlain(r) }))}` }), by: (r) => ({ file: ".vscode/mcp.json", text: json({ servers: { avp: { type: "stdio", ...mcpPlain(r) } } }), note: "The Copilot CLI reads .github/mcp.json." }) },
  { id: "gemini", name: "Gemini CLI", kind: "Terminal", run: "gemini", by: (r) => ({ file: ".gemini/settings.json", text: json({ mcpServers: { avp: mcpPlain(r) } }), note: "Loaded in a trusted folder." }) },
  { id: "windsurf", name: "Windsurf", kind: "Editor", run: "windsurf .", by: (r) => ({ file: ".devin/mcp_config.json", text: json({ mcpServers: { avp: mcpPlain(r) } }) }) },
  { id: "opencode", name: "OpenCode", kind: "Terminal", run: "opencode", by: (r) => ({ file: "opencode.json", text: json({ mcp: { avp: { type: "local", command: ["avp", "mcp"], enabled: true, environment: mcpEnv(r) } } }) }) },
  { id: "amp", name: "Amp", kind: "Terminal", run: "amp", by: (r) => ({ file: ".amp/settings.json", text: json({ "amp.mcpServers": { avp: mcpPlain(r) } }), note: "Approve it once: amp mcp approve avp." }) },
  { id: "aider", name: "Aider", kind: "Terminal", run: "aider", by: (r) => ({ how: "Aider has no MCP: it runs avp in its shell, from the rule avp up writes (.aider.avp.md). Or by hand, for one task:", text: `avp open --repo ${r || "owner/repo"} --goal "what this change does"\n# edit, then\navp land` }) },
];
const connectHref = (repo, agent) => {
  const q = new URLSearchParams();
  if (repo) q.set("repo", repo);
  if (agent) q.set("agent", agent);
  const s = q.toString();
  return `/connect${s ? `?${s}` : ""}`;
};
// The agents as a strip of tiles, each one a link to its page (the home and a repo's side).
const agentTiles = (repo) => `<div class="agent-tiles">${CONNECT.map((a) => `<a href="${connectHref(repo, a.id)}" aria-label="Connect ${esc(a.name)}">${mark(a.id)}<span>${esc(a.name.replace(" CLI", ""))}</span></a>`).join("")}</div>`;
function closeSheet() {}
document.addEventListener("click", (ev) => {
  const b = ev.target.closest("[data-connect]");
  if (b) go(connectHref(b.dataset.connect));
});

page(/^\/connect$/, async (el, _, q, n) => {
  crumbs([["Connect an agent"]]);
  const list = await reposKnown();
  if (!current(n)) return;
  let target = q.get("repo") || list[0] || "";
  let on = q.get("agent") || (() => { try { return localStorage.getItem("avp.agent"); } catch { return null; } })() || "claude";
  if (!CONNECT.some((a) => a.id === on)) on = "claude";
  const draw = () => {
    const a = CONNECT.find((x) => x.id === on);
    const by = a.by(target);
    const folder = target ? target.split("/").pop() : "";
    const app = a.app && a.app(target);
    const start = target ? `avp clone ${target} && cd ${folder} && ${a.run || "avp up"}` : `cd your-project && avp up && ${a.run || ""}`.replace(/ && $/, "");
    const all = `${INSTALL} && avp login ${ORIGIN}`;
    el.innerHTML = `
      <div class="pagehead"><div class="grow"><h1>Connect an agent</h1><p class="soft">Once per machine: after that, your agent opens tasks and lands them proven, on its own.</p></div>
        ${list.length > 1 ? `<label class="inline">Repository<select id="c-repo">${list.map((r) => `<option${r === target ? " selected" : ""}>${esc(r)}</option>`).join("")}</select></label>` : target ? `<span class="pill">${icon("repo")}${esc(target)}</span>` : ""}</div>
      <div class="agent-pick" role="tablist" aria-label="Agent">${CONNECT.map((x) => `<button type="button" role="tab" aria-selected="${x.id === on}" data-agent="${x.id}">${mark(x.id, "lg")}<span class="who"><strong>${esc(x.name)}</strong><span>${esc(x.kind)}</span></span></button>`).join("")}</div>
      <section class="card connect" role="tabpanel" aria-label="${esc(a.name)}">
        <div class="connect-head">${mark(a.id, "xl")}<div class="grow"><h2>${esc(a.name)}</h2><p class="soft">${app ? `${esc(a.name)} installs the avp server in one click: it asks you to confirm.` : `One line in your terminal: it lays ${target ? `<strong>${esc(target)}</strong>` : "your project"} out, wires ${esc(a.name)} and starts it.`}</p></div>
          ${app ? `<a class="button primary big" href="${esc(app.href)}">${icon("out")}${esc(app.label)}</a>` : `<button type="button" class="primary big" data-copy="${esc(start)}">${icon("copy")}Copy the start command</button>`}</div>
        <ol class="steps-n">
          <li><strong>Install avp and sign this machine in</strong><p>Once per machine. The login opens a page here to approve the terminal.</p>${cmd(all)}</li>
          <li><strong>${app ? `Open ${esc(a.name)}` : `Start ${esc(a.name)}`}</strong><p>${app ? `Then open ${target ? `a checkout of <strong>${esc(target)}</strong>` : "your project"} in it: every task goes through AVP.` : `${target ? `<code>avp clone</code> lays the head out and wires every agent it finds` : `<code>avp up</code> finds ${esc(a.name)} and writes its config`}.`}</p>${app ? cmd(target ? `avp clone ${target}` : "avp up") : cmd(start)}</li>
        </ol>
        <details class="byhand"><summary>${esc(by.how || `Or by hand, in ${by.file}`)}</summary>${cmd(by.text)}${by.note ? `<p class="small soft">${esc(by.note)}</p>` : ""}</details>
      </section>
      <p class="small soft connect-foot">Any other agent with MCP: the same server, <code>avp mcp</code>. <button type="button" class="link" data-copy="${esc(json({ mcpServers: { avp: mcpPlain(target) } }))}">Copy the MCP config</button></p>`;
    $("c-repo")?.addEventListener("change", (ev) => { target = ev.target.value; history.replaceState(null, "", connectHref(target, on)); draw(); });
  };
  el.onclick = (ev) => {
    const b = ev.target.closest("[data-agent]");
    if (!b) return;
    on = b.dataset.agent;
    try { localStorage.setItem("avp.agent", on); } catch {}
    history.replaceState(null, "", connectHref(target, on));
    draw();
  };
  draw();
});

// ---- the router ----

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

// ---- the field: the landing's dotted clouds, quiet, behind Activity and the home feed only ----
// One canvas for the site. The clouds drift slowly, so a dozen frames a second is plenty; none while
// the tab is hidden or another page shows, and one still frame under reduced motion.
const field = (() => {
  const still = matchMedia("(prefers-reduced-motion: reduce)");
  const SPEC = [[.06, .18, .2, .9], [.32, .42, .12, -.6], [.94, .1, .18, -.7], [.74, .5, .14, .8], [.5, .02, .12, .5], [.16, .78, .14, -.4]];
  let c = null, g = null, W = 0, H = 0, on = false, raf = 0, last = -1e9;
  const size = () => { const d = Math.min(devicePixelRatio || 1, 2); W = c.clientWidth; H = c.clientHeight; c.width = W * d; c.height = H * d; g.setTransform(d, 0, 0, d, 0, 0); };
  const paint = (t) => { g.clearRect(0, 0, W, H); clouds(g, W, H, t / 1000, SPEC, { S: 6, rgb: getComputedStyle(c).getPropertyValue("--avp-dot").trim(), alpha: .2, gain: 1.2, floor: .3 }); };
  const frame = (t) => {
    raf = 0;
    if (!on || document.hidden) return;
    if (t - last > 80) { last = t; paint(t); }
    if (!still.matches) raf = requestAnimationFrame(frame);
  };
  const wake = () => { if (on && !raf && !document.hidden) raf = requestAnimationFrame(frame); };
  document.addEventListener("visibilitychange", wake);
  still.addEventListener("change", wake);
  addEventListener("resize", () => { if (on) { size(); last = -1e9; wake(); } });
  return (show) => {
    if (show && !c) { c = document.createElement("canvas"); c.className = "field"; c.setAttribute("aria-hidden", "true"); g = c.getContext("2d"); $("shell").prepend(c); }
    if (!c || on === show) return;
    on = show;
    c.classList.toggle("on", on);
    if (on) { size(); last = -1e9; wake(); } else if (raf) { cancelAnimationFrame(raf); raf = 0; }
  };
})();

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
  // A repo's pages keep its navigation while the next one loads; any other page has none.
  if (!/^\/r\//.test(path)) { $("side").hidden = true; $("shell").classList.remove("with-side"); }
  closeSheet();
  field(false);
  try {
    await Promise.all([me(), INSTALL_KNOWN]);
  } catch {}
  drawMe();
  const inRepo = path.match(/^\/r\/(.+?)(?:\/(?:code|history|line|p|tasks|proofs|envs|activity|settings)(?:\/|$)|$)/);
  drawTools(inRepo ? decodeURIComponent(inRepo[1]) : "");
  if (!route) {
    crumbs([]);
    el.innerHTML = failed(new ApiError(404, `No page at ${path}.`), false);
    return;
  }
  if (!route.open && !signedIn()) return toSignIn();
  // A part the address leaves out (the repo's root folder) is "", never "undefined".
  const params = path.match(route.re).slice(1).map((x) => (x == null ? "" : decodeURIComponent(x)));
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
    <span class="eyebrow">AVP Cloud</span>
    <h1>Welcome <span class="soft">back.</span></h1>
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
    <span class="eyebrow">Start free</span>
    <h1>Where agents <span class="soft">ship.</span></h1>
    <p class="soft">Tasks instead of branches, every change proven before it lands. 3,000 proof minutes a month, free.</p>
    ${m.github ? `<a class="button wide" href="/auth/github?next=${enc(nextOf(q))}&signup=1">Sign up with GitHub</a><div class="or">or</div>` : ""}
    <form id="su">
      <label>Name <input name="name" autocomplete="username" required pattern="${REPO_NAME}" title="Letters, digits, dots, dashes" placeholder="ada"></label>
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
    <span class="eyebrow">avp login</span>
    <h1>Sign in a <span class="soft">terminal.</span></h1>
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

// The proof minutes of a space this month, against its plan's: a number, a meter, the day it starts again.
function usageCard(u, title = "Proof minutes this month") {
  if (!u || u.space === null) return "";
  const pct = u.cap ? Math.min(100, (100 * u.minutes) / u.cap) : 0;
  return `<div class="card usage"><span class="label">${esc(title)}</span>
    <p class="big">${Math.round(u.minutes).toLocaleString(LOCALE)}<span class="soft"> ${u.cap != null ? `of ${Math.round(u.cap).toLocaleString(LOCALE)} min` : "min"}</span></p>
    ${u.cap ? `<div class="meter${u.over ? " over" : ""}"><i style="width:${pct.toFixed(1)}%"></i></div>` : ""}
    <p class="small soft">${u.over ? "<strong>Past the cap: new proofs wait for the month to end or a bigger plan.</strong> " : ""}${esc(u.plan || "free")} plan${u.cap == null ? ", no cap" : ""} · starts again ${when(Date.parse(u.resets_at))}</p></div>`;
}

// A repo's name as a person reads it: its space faint, the name strong.
// A commit brought in from Git reads "7fd1a60 Its message (Its author, 2012-03-06)": its message
// alone as the title, the author and date apart.
const IMPORTED = /^([0-9a-f]{7,40}) (.*) \(([^()]+), (\d{4}-\d{2}-\d{2})\)$/s;
const imported = (note) => { const m = String(note || "").match(IMPORTED); return m ? { sha: m[1], title: m[2], author: m[3], date: m[4] } : null; };
const noteTitle = (note) => imported(note)?.title || note || "";
const repoLabel = (name) => { const i = name.lastIndexOf("/"); return i < 0 ? `<strong>${esc(name)}</strong>` : `<span class="soft">${esc(name.slice(0, i + 1))}</span><strong>${esc(name.slice(i + 1))}</strong>`; };

// ---- the line's events, said as a person reads them (Activity, and the home across repos) ----

// What one event says: its sentence, its mark (an icon, or a proof's color), the position it is
// about. Null for the events no one needs to read (writes, leases).
function eventPhrase(e, goal) {
  const d = e.data || {};
  const g = `<strong>${esc(short(goal || d.goal || "a task", 90))}</strong>`;
  const env = `<strong>${esc(d.env || "")}</strong>`;
  const by = esc(d.by === "avp" ? "AVP" : d.by || "");
  switch (e.kind) {
    case "task.opened": return { glyph: "tasks", text: `${esc(agentName(d.actor?.agent))}${d.actor?.person ? ` (${esc(d.actor.person)})` : ""} opened ${g}` };
    case "task.landed": return d.position == null ? null : { tone: "green", text: `Landed ${g}`, pos: d.position, proof: d.proof };
    case "task.replayed": return { glyph: "history", text: `${g} replayed on the new head${(d.conflicts || []).length ? `, ${plural(d.conflicts.length, "conflict")}` : ""}` };
    case "task.awaiting_approval": return { tone: "wait", text: `${g} waits for a person's approval` };
    case "task.approved": return { glyph: "proofs", text: `${esc(d.person || "A person")} approved ${g}` };
    case "task.refused": return { tone: "red", text: `${esc(d.person || "A person")} refused ${g}` };
    case "task.abandoned": return { glyph: "x", text: `Abandoned ${g}` };
    case "proof": return d.verdict ? { tone: d.verdict, text: `Proof ${esc(d.verdict)} for ${g}`, proof: d.proof } : null;
    case "env.promoted": return { glyph: "up", text: `${by || "Someone"} promoted ${chipText(d.position)} to ${env}`, pos: d.position };
    case "deploy.dispatched": return { tone: "running", text: `Deploying ${chipText(d.position)} to ${env}`, pos: d.position };
    case "env.moved": return { tone: "green", text: `${env} serves ${chipText(d.position)}`, pos: d.position };
    case "env.failed": return { tone: "red", text: `Deploy of ${chipText(d.position)} to ${env} failed${d.exit != null ? ` (exit ${esc(d.exit)})` : d.reason ? `: ${esc(d.reason)}` : ""}`, pos: d.position };
    case "env.interrupted": return { tone: "wait", text: `Deploy of ${chipText(d.position)} to ${env} interrupted`, pos: d.position };
    case "line.green": return { tone: "green", text: `Full suite green on ${chipText(d.position)}`, pos: d.position, proof: d.proof };
    case "line.red": return { tone: "red", text: `Full suite red on ${chipText(d.position)}`, pos: d.position, proof: d.proof };
    case "line.flaky": return { tone: "wait", text: `A flaky test on ${chipText(d.position)}${d.shard ? ` (${esc(d.shard)})` : ""}`, pos: d.position };
    case "line.reverted": return { tone: "red", text: `${chipText(d.position)} taken back: ${esc(short(d.goal, 70))}`, pos: d.position };
    case "mirror.blocked": return { tone: "red", text: `The mirror to ${esc(d.branch)} is blocked` };
    default: return null;
  }
}
const chipText = (pos) => `<span class="chip">#${esc(pos)}</span>`;
// A phrase's mark: a proof's dot when it has a verdict, else its icon.
const eventMark = (ph) => ph.tone ? verdictDot(ph.tone === "wait" ? "" : ph.tone) : icon(ph.glyph || "activity");
// Goals of the tasks the events name, from their own `task.opened`.
function goalsOf(evs, into = new Map()) {
  for (const e of evs) if (e.task && e.data?.goal) into.set(e.task, e.data.goal);
  return into;
}

page(/^\/$/, async (el, _, q, n) => {
  crumbs([]);
  const [repos, os, invites, usage] = await Promise.all([api("/repos"), orgs().catch(() => null), maybe("/invites").catch(() => null),
    ME.person && ME.cloud !== false ? maybe(`/orgs/${enc(ME.person)}/usage`).catch(() => null) : null]);
  if (!current(n)) return;
  REPOS = Promise.resolve(repos.map(fullName).sort());
  // The head of every repo, for the list.
  const lines = await Promise.all(repos.map((r) => api(`/repos/${repoPath(fullName(r))}/positions?limit=1`).catch(() => [])));
  if (!current(n)) return;
  const hello = ME.person ? `Hello, ${esc(ME.person)}` : "This server";
  if (!repos.length) {
    el.innerHTML = `<section class="hero">
        <span class="eyebrow">Welcome${ME.person ? `, ${esc(ME.person)}` : ""}</span>
        <h1>Put your first project <span class="soft">on AVP.</span></h1>
        <p class="soft lead">A repo here is a line of positions: each one a task an agent opened, wrote and landed, proven on the exact head first. Start one now, your agents take it from there.</p>
        <div class="bar"><a class="button primary big" href="/new">Create a repo</a><button type="button" class="big" data-connect="">${icon("plug")}Connect an agent</button></div>
      </section>
      <div class="split"><section class="card boxed"><div class="chead"><h2>Or from a project's folder</h2></div><div class="cbody">${setupCommands().map(cmd).join("")}</div></section>
        <aside class="stack">${invitesCard(invites)}${usageCard(usage)}${os?.length ? orgList(os) : ""}</aside></div>`;
    answerInvites(el);
    return;
  }
  const heads = new Map(repos.map((r, i) => [r.id, lines[i][0] || null]));
  const sorted = [...repos].sort((a, b) => (heads.get(b.id)?.landed_at || b.created_at) - (heads.get(a.id)?.landed_at || a.created_at));
  const repoItem = (r) => {
    const h = heads.get(r.id), name = fullName(r);
    return `<a class="item" href="/r/${repoPath(name)}" data-name="${esc(name.toLowerCase())}">${avatar(r.name, "sq")}<span class="main"><span class="title">${repoLabel(name)}</span>
      <span class="meta">${h ? esc(short(noteTitle(h.note), 90)) : "Nothing on the line yet"}</span></span>
      <span class="side">${h?.proof ? verdictPill(h.verdict) : ""} <span class="hide-sm">${h ? chipText(h.position) : ""}</span> ${timeAgo(h ? h.landed_at : r.created_at)}</span></a>`;
  };
  const last = sorted.map((r) => heads.get(r.id)).find(Boolean);
  el.innerHTML = `
    <div class="pagehead"><div class="grow"><h1>${hello}</h1><p class="soft">${plural(repos.length, "repo")}${last ? ` · last landing ${ago(last.landed_at)}` : ""}</p></div>
      <a class="button primary" href="/new">${icon("plus")}New repo</a></div>
    <div class="split">
      <section class="stack">
        <div class="card boxed"><div class="chead"><h2>Repositories <span class="count">${repos.length}</span></h2>${repos.length > 6 ? `<input id="find" class="find" placeholder="Find a repo" aria-label="Find a repo">` : ""}</div>
          <div class="list" id="repos">${sorted.map(repoItem).join("")}</div></div>
        <div class="card boxed"><div class="chead"><h2>Activity</h2><span class="live-dot" id="feed-live">Live</span></div><div id="feed">${loading("Reading the lines")}</div></div>
      </section>
      <aside class="stack">${invitesCard(invites)}${usageCard(usage)}${os ? orgList(os) : ""}
        <div class="card"><span class="label">Connect your agents</span><p class="soft small">Once each: it opens tasks and lands them, proven, on its own.</p>
          ${agentTiles("")}<a class="button wide" href="/connect">${icon("plug")}Connect an agent</a></div>
      </aside>
    </div>`;
  $("find")?.addEventListener("input", (ev) => {
    const w = ev.target.value.trim().toLowerCase();
    $("repos").querySelectorAll("a.item").forEach((a) => { a.hidden = !!w && !a.dataset.name.includes(w); });
  });
  answerInvites(el);
  field(true);
  homeFeed(sorted.slice(0, 8).map(fullName), n);
}, false);

// The home's feed: the newest events of the repos last moved, read again every few seconds while
// the page is shown (and the tab visible).
async function homeFeed(names, n) {
  const goals = new Map();
  let seen = null, known = null;
  while (current(n)) {
    if (!document.hidden) {
      const all = await Promise.all(names.map((r) => api(`/repos/${repoPath(r)}/events?last=40`, { quiet: true }).then((evs) => evs.map((e) => ({ ...e, repo: r }))).catch(() => null)));
      if (!current(n)) return;
      const box = $("feed");
      $("feed-live")?.classList.toggle("off", all.every((x) => x === null));
      const evs = all.flatMap((x) => x || []);
      for (const r of names) goalsOf(evs.filter((e) => e.repo === r), goals);
      const rows = evs.sort((a, b) => b.at - a.at).map((e) => [e, eventPhrase(e, goals.get(e.task))]).filter(([, ph]) => ph).slice(0, 14);
      const key = rows.map(([e]) => `${e.repo}:${e.id}`).join(",");
      if (box && key !== seen) {
        seen = key;
        // What arrived since the last read slides in; the first read just shows.
        const was = known;
        known = new Set(rows.map(([e]) => `${e.repo}:${e.id}`));
        box.innerHTML = rows.length ? `<div class="rows" style="border:0;border-radius:0">${rows.map(([e, ph]) => {
          const href = `/r/${repoPath(e.repo)}${ph.pos != null ? `/p/${ph.pos}` : "/activity"}`;
          return `<a class="row${was && !was.has(`${e.repo}:${e.id}`) ? " fresh" : ""}" href="${href}"><span class="glyph">${eventMark(ph)}</span><span class="what"><span class="title">${ph.text}</span><span class="meta">${esc(e.repo)}</span></span><span class="end">${timeAgo(e.at)}</span></a>`;
        }).join("")}</div>` : empty("Nothing moved yet", "The first task an agent opens shows up here.", [], { icon: "activity" });
      }
    }
    await new Promise((ok) => setTimeout(ok, 8000));
  }
}

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
  return `<div class="card"><span class="label">Organizations</span>${os.length ? `<div class="list">${os.map((o) => `<a class="item" href="/o/${enc(o.name)}"><span class="main"><span class="title">${esc(o.name)}</span><span class="meta">${esc(o.plan || "")}</span></span></a>`).join("")}</div>` : `<p class="soft small">None yet.</p>`}</div>`;
}

// A repo's name: what POST /repos takes in a space (letters, digits, dots, dashes, underscores).
// The `-` escaped: a browser reads `pattern` with the v flag, where a bare one is refused.
const REPO_NAME = String.raw`[A-Za-z0-9][A-Za-z0-9._\-]*`;

page(/^\/new$/, async (el, _, q) => {
  crumbs([["New repo"]]);
  // Where it may live: the person's own space, or an organization they administer.
  const owners = ME.person && ME.cloud !== false ? [ME.person, ...(ME.orgs || []).filter((o) => ["admin", "owner"].includes(o.role)).map((o) => o.org)] : [];
  const pick = q.get("owner");
  el.className = "narrow wide-form";
  el.innerHTML = `
    <span class="eyebrow">New repo</span>
    <h1>Start a <span class="soft">repo.</span></h1>
    <form id="mk" class="card form">
      <div class="row">
        ${owners.length ? `<label class="owner">Owner <select name="owner">${owners.map((o) => `<option${o === pick ? " selected" : ""}>${esc(o)}</option>`).join("")}</select></label><span class="slash">/</span>` : ""}
        <label class="grow">Name <input name="name" required pattern="${REPO_NAME}" title="Letters, digits, dots, dashes" placeholder="shop" autocomplete="off" autofocus></label>
      </div>
      <fieldset><legend>Start with</legend>
        <label class="choice"><input type="radio" name="start" value="readme" checked><span><strong>A README</strong><span class="soft small">A README.md with the repo's name, ready to edit.</span></span></label>
        <label class="choice"><input type="radio" name="start" value="empty"><span><strong>Nothing</strong><span class="soft small">An agent or you lands the first files.</span></span></label>
      </fieldset>
      <label id="about-box">What it is <input name="about" placeholder="One line for the README" autocomplete="off"></label>
      <p class="small" id="err" role="alert"></p>
      <button class="primary wide">Create repo</button>
    </form>
    <h2 id="terminal" class="section">Or from your terminal</h2>
    <p class="soft">From the project's folder, a Git checkout or not. <code>avp up</code> creates the repo from its history, reads its CI into the checks every landing proves, and wires the coding agents it finds.</p>
    ${setupCommands().map(cmd).join("")}
    <p class="small soft">The second command opens <a href="/login/device">a page here</a> to approve the terminal.</p>
    <h2 class="section">Or import a Git repository</h2>
    <p class="soft">Any Git URL or <code>github.com/owner/name</code>: each commit becomes a position, the history comes along, <code>git clone</code> still reads it.</p>
    ${cmd("avp import github.com/owner/name")}`;
  const f = $("mk");
  f.addEventListener("change", () => { $("about-box").hidden = f.start.value !== "readme"; });
  f.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const b = f.querySelector("button.primary");
    const name = f.name.value.trim();
    const owner = f.owner?.value;
    const full = owner && owner !== ME.person ? `${owner}/${name}` : name;
    const about = f.about.value.trim();
    const body = f.start.value === "readme"
      ? { name: full, readme: `# ${name}\n\n${about || "A repo on AVP: agents open tasks on its line, write, and land them, each one proven first."}\n` }
      : { name: full, start: true };
    b.disabled = true;
    try {
      const r = await api("/repos", { method: "POST", body, quiet: true });
      if (about) await api(`/repos/${repoPath(fullName(r))}/settings/general`, { method: "POST", body: { description: about }, quiet: true }).catch(() => {});
      go(`/r/${repoPath(fullName(r))}`);
    } catch (e) {
      $("err").textContent = e.message;
      b.disabled = false;
    }
  });
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
  const plan = org.plan || "free";
  const PLANS = { free: ["Free", "3 people, 3,000 proof minutes a month"], team: ["Team", "$10 a person a month, 5,000 proof minutes each, pooled"], enterprise: ["Enterprise", "Your own terms, SSO and support"] };
  el.innerHTML = `
    <div class="pagehead">${avatar(org.name, "sq xl")}<div class="grow"><h1>${esc(org.name)}</h1><p class="soft">Organization · ${plural(repos.length, "repo")} · ${plural(mems.length, "member")}</p></div>
      <span class="pills"><span class="pill tag">${esc((PLANS[plan] || [plan])[0])}</span>${org.role ? `<span class="pill">${esc(org.role)}</span>` : ""}</span><a class="button primary" href="/new">${icon("plus")}New repo</a></div>
    <div class="split">
      <section class="stack">
        <div class="card boxed"><div class="chead"><h2>Repositories <span class="count">${repos.length}</span></h2></div>${repos.length ? `<div class="list">${repos.map((r) => `<a class="item" href="/r/${repoPath(`${org.name}/${r}`)}">${avatar(r, "sq")}<span class="main"><span class="title">${repoLabel(`${org.name}/${r}`)}</span></span>${icon("right")}</a>`).join("")}</div>` : `<div class="cbody">${empty("No repo yet", "Create one here, or bring a project's folder:", [`avp org repo ${org.name} <repo>`], { href: "/new", action: "Create a repo" })}</div>`}</div>
        <div class="card boxed"><div class="chead"><h2>Members</h2><span class="soft">${esc(org.seats_said || plural(mems.length, "seat"))}</span></div><div class="list">${mems.map((m) => `<div class="item">${avatar(m.person, "lg")}<span class="main"><span class="title">${esc(m.person)}</span></span><span class="pill">${esc(m.role)}</span></div>`).join("")}
          ${invs.map((i) => `<div class="item">${avatar(i.person || i.email, "lg")}<span class="main"><span class="title">${esc(i.person || i.email)}</span><span class="meta">invited as ${esc(i.role)} by ${esc(i.by)} · ${ago(i.at)}</span></span><span class="pill run">waiting</span></div>`).join("")}</div>
          ${admin ? `<div class="cfoot"><form id="inv" class="invite-form"><input name="to" placeholder="A name or teammate@example.com" aria-label="Who to invite" required><select name="role" aria-label="Role"><option>member</option><option>admin</option><option>owner</option></select><button class="primary">${icon("plus")}Invite</button></form><p class="small" id="err" role="alert"></p></div>` : ""}</div>
      </section>
      <aside class="stack">
        <div class="card"><span class="label">Proof minutes this month</span>${u ? `
          <p class="big">${Math.round(u.minutes).toLocaleString(LOCALE)}${u.cap != null ? ` <span class="soft">of ${Math.round(u.cap).toLocaleString(LOCALE)} min</span>` : ""}</p>
          ${u.cap ? `<div class="meter${u.over ? " over" : ""}"><i style="width:${Math.min(100, (100 * u.minutes) / u.cap).toFixed(1)}%"></i></div>` : ""}
          <p class="small soft" style="margin:0">${Math.round(u.proof_minutes)} min of proofs, ${Math.round(u.full_minutes)} of full suites. ${u.over ? "<strong>Past the cap: new proofs wait for the month to end or a bigger plan.</strong> " : ""}${u.cap == null ? "No cap on this plan. " : ""}Starts again ${when(Date.parse(u.resets_at))}.</p>` : `<p class="soft small" style="margin:0">Nothing measured yet.</p>`}</div>
        <div class="card plan-card"><span class="label">Plan</span><p class="big">${esc((PLANS[plan] || [plan])[0])}</p><p class="small soft">${esc((PLANS[plan] || [, ""])[1])}</p>
          ${ME.admin ? `<form id="plan" class="invite-form"><select name="plan" aria-label="Plan">${Object.entries(PLANS).map(([k, [l]]) => `<option value="${k}"${k === plan ? " selected" : ""}>${l}</option>`).join("")}</select><button>Change</button></form>`
            : plan === "free" ? `<button type="button" class="wide" disabled>Team plan: soon</button>` : ""}</div>
      </aside>
    </div>`;
  $("plan")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    try { await api(`/orgs/${enc(name)}/plan`, { method: "POST", body: { plan: new FormData(ev.target).get("plan") } }); toast("Plan changed"); render(); } catch (e) { toast(e.message); }
  });
  $("inv")?.addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    try {
      await api(`/orgs/${enc(name)}/invites`, { method: "POST", body: { to: f.get("to").trim(), role: f.get("role") } });
      render();
    } catch (e) { $("err").textContent = e.message; }
  });
});

// ---- usage: the proof minutes of every space the person works in, this month ----

page(/^\/usage$/, async (el, _, q, n) => {
  crumbs([["Usage"]]);
  const spaces = ME.person ? [ME.person, ...(ME.orgs || []).map((o) => o.org)] : [];
  const all = await Promise.all(spaces.map((s) => maybe(`/orgs/${enc(s)}/usage`).catch(() => null)));
  if (!current(n)) return;
  const known = all.map((u, i) => [spaces[i], u]).filter(([, u]) => u && u.space !== null);
  el.innerHTML = `<div class="pagehead"><div class="grow"><h1>Usage</h1><p class="soft">Proof minutes this month: what every landing and full suite ran on the workers, counted by the space its repo lives in. The free plan has 3,000 a month.</p></div></div>` + (known.length
    ? `<div class="list">${known.map(([s, u]) => `<a class="item" href="${s === ME.person ? "/" : `/o/${enc(s)}`}" style="flex-wrap:wrap"><span class="main"><span class="title"><strong>${esc(s)}</strong> <span class="soft small">${s === ME.person ? "your own repos" : "organization"}</span></span>
        <span class="meta">${esc(u.plan || "free")} plan · ${plural(u.repos?.length || 0, "repo")} · starts again ${when(Date.parse(u.resets_at))}</span></span>
        <span class="side">${Math.round(u.minutes).toLocaleString(LOCALE)} ${u.cap != null ? `of ${Math.round(u.cap).toLocaleString(LOCALE)} min` : "min, no cap"}${u.over ? `<br><span class="pill red">past the cap</span>` : ""}</span>
        ${u.cap ? `<div class="meter${u.over ? " over" : ""}" style="flex-basis:100%"><i style="width:${Math.min(100, (100 * u.minutes) / u.cap).toFixed(1)}%"></i></div>` : ""}</a>`).join("")}</div>`
    : empty("Nothing measured here", "This server counts no proof minutes: it has no plans."));
});

// ---- a person's own settings: their tokens ----

page(/^\/settings$/, async (el, _, q, n) => {
  crumbs([["Account"]]);
  if (!ME.accounts) {
    el.innerHTML = empty("No accounts on this server", "Whoever reaches it may do everything: tokens and roles come with the first <code>avp user add</code>.", [], { icon: "user" });
    return;
  }
  const tokens = await api("/tokens");
  if (!current(n)) return;
  const alive = tokens.filter((t) => !t.revoked_at);
  // A web sign-in is a session token: listed apart, as the browsers signed in, not among the tokens.
  const live = alive.filter((t) => t.kind !== "session"), sessions = alive.filter((t) => t.kind === "session");
  const roles = ME.roles || [];
  el.innerHTML = `
    <div class="card profile" style="margin-bottom:var(--avp-space-5)">${avatar(ME.person, "xl")}<div class="grow"><h1>${esc(ME.person)}</h1>
      <p class="soft">${ME.email ? `${esc(ME.email)} · ` : ""}${plural(roles.length, "repo")} with a role${ME.orgs?.length ? ` · ${plural(ME.orgs.length, "organization")}` : ""}</p></div>
      ${ME.admin ? `<span class="pill tag">Server admin</span>` : ""}</div>
    <div class="stack">
    <section class="card boxed"><div class="chead"><h2>Tokens</h2><span class="soft">${plural(live.length, "token")}</span></div>
      <div class="cbody"><p class="soft small">What a terminal, an agent or a machine of yours signs in with (<code>AVP_TOKEN</code>). <code>avp login</code> makes one for you; make one by hand for a CI. Shown once when made, kept hashed.</p>
        <form id="mk" class="token-form"><label>Name<input name="name" placeholder="laptop, ci, an agent" required autocomplete="off"></label>
          <label>Expires<select name="expires"><option value="90d">In 90 days</option><option value="30d">In 30 days</option><option value="never">Never</option></select></label>
          <button class="primary">${icon("plus")}Create token</button></form>
        <div id="new"></div></div>
      ${live.length ? `<div class="scroll-x"><table><tr><th>Name</th><th class="hide-sm">Kind</th><th class="hide-sm">Made</th><th>Last used</th><th class="hide-sm">Expires</th><th class="end"></th></tr>${live.map((t) => `<tr>
        <td><strong>${esc(t.name)}</strong><div>${idChip(t.id)}</div></td><td class="hide-sm"><span class="pill">${esc(t.kind)}</span></td><td class="hide-sm">${timeAgo(t.created_at)}</td>
        <td>${t.used_at ? timeAgo(t.used_at) : `<span class="soft">never</span>`}</td><td class="hide-sm">${t.expires_at ? `<span class="soft">${esc(when(t.expires_at))}</span>` : `<span class="soft">never</span>`}</td>
        <td class="end"><button class="danger sm" data-revoke="${esc(t.id)}">Revoke</button></td></tr>`).join("")}</table></div>`
        : `<div style="border-top:var(--avp-border) solid var(--avp-line)">${empty("No token yet", "Make one for each machine, so one lost is one revoked.", [], { icon: "key" })}</div>`}
    </section>
    ${sessions.length ? `<section class="card boxed"><div class="chead"><h2>Signed-in browsers</h2><span class="soft">${plural(sessions.length, "session")}</span></div>
      <div class="list">${sessions.map((t) => `<div class="item"><span class="glyph-s">${icon("user")}</span><span class="main"><span class="title">${esc(t.name.replace(/^AVP Live \(password\)$/, "Signed in with a password"))}</span><span class="meta">Since ${esc(when(t.created_at))} · last seen ${esc(ago(t.used_at || t.created_at))}${t.expires_at ? ` · ends ${esc(when(t.expires_at))}` : ""}</span></span><button class="ghost sm" data-revoke="${esc(t.id)}">Sign out</button></div>`).join("")}</div></section>` : ""}
    <section class="card boxed"><div class="chead"><h2>Repositories you have a role on</h2></div>
      ${roles.length ? `<div class="scroll-x"><table><tr><th>Repository</th><th class="end">Role</th></tr>${roles.map((r) => `<tr><td><a href="/r/${repoPath(r.repo)}">${repoLabel(r.repo)}</a></td><td class="end"><span class="pill">${esc(r.role)}</span></td></tr>`).join("")}</table></div>`
        : `<div class="cbody soft small">None yet: the repos you create are yours, those of others come with an invitation.</div>`}
    </section></div>`;
  $("mk").addEventListener("submit", async (ev) => {
    ev.preventDefault();
    const f = new FormData(ev.target);
    try {
      const t = await api("/tokens", { method: "POST", body: { name: f.get("name"), expires: f.get("expires") } });
      await render();
      $("new").innerHTML = `<div class="notice" style="margin:var(--avp-space-4) 0 0"><strong>${esc(t.name)}</strong>: copy it now, it is not shown again.${cmd(`export AVP_TOKEN=${t.token}`)}<p class="small soft" style="margin:var(--avp-space-2) 0 0">For git, it is the password, with any user name.</p></div>`;
    } catch (e) { toast(e.message); }
  });
  el.querySelectorAll("[data-revoke]").forEach((b) => b.addEventListener("click", async () => {
    if (b.classList.contains("danger") && !confirm("Revoke this token? What signs in with it stops at once.")) return;
    try { await api(`/tokens/${enc(b.dataset.revoke)}/revoke`, { method: "POST", body: {} }); render(); } catch (e) { toast(e.message); }
  }));
});

addEventListener("DOMContentLoaded", () => render());
