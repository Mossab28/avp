//! AVP Cloud (`AVP_CLOUD=1`): one server for many organizations that do not know each other. An
//! organization has members, each with an org role (member, admin, owner) that gives its repos'
//! role unless a role on the repo says otherwise, invitations a person accepts, a plan (free, team,
//! enterprise) and the proof minutes its repos used this calendar month. A repo belongs to an
//! organization or to the person who created it (their own space, metered as theirs). A team server
//! without `AVP_CLOUD=1` has none of this and works as before.

use super::auth::{self, Forbidden, Role, Who};
use crate::db::Db;
use crate::engine::Engine;
use crate::model::{new_id, now_ms, Repo};
use crate::stats::{parse_time, rfc3339, tenth};
use anyhow::{anyhow, Result};
use axum::extract::{Path as P, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Where organizations' events go: the server's, as people's (an org's are read by its admins).
const SERVER: &str = "";

/// AVP Cloud is on: sign-up, organizations, plans and their minutes, and objects kept to the repos
/// that hold them.
pub fn on() -> bool {
    std::env::var("AVP_CLOUD").is_ok_and(|v| v.trim() == "1")
}

/// Anyone may make an account (`AVP_SIGNUP=open`, on AVP Cloud only).
pub fn signup_open() -> bool {
    on() && std::env::var("AVP_SIGNUP").is_ok_and(|v| v.trim() == "open")
}

fn needs_cloud() -> Result<()> {
    anyhow::ensure!(on(), "organizations are AVP Cloud's: this server runs without AVP_CLOUD=1 (a team server gives roles per repo: `avp user role`)");
    Ok(())
}

/// What a member may do in an organization, each role holding the ones below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrgRole {
    Member,
    Admin,
    Owner,
}

impl OrgRole {
    /// A role by name; `none` is no role (out of the organization).
    pub fn parse(s: &str) -> Result<Option<OrgRole>> {
        match s.trim() {
            "member" => Ok(Some(OrgRole::Member)),
            "admin" => Ok(Some(OrgRole::Admin)),
            "owner" => Ok(Some(OrgRole::Owner)),
            "none" => Ok(None),
            other => Err(anyhow!("{other:?} is no org role: member (write on its repos), admin (admin on its repos, members, invitations), owner (and its owners), or none")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            OrgRole::Member => "member",
            OrgRole::Admin => "admin",
            OrgRole::Owner => "owner",
        }
    }

    /// The role it gives on the organization's repos, where the repo gives none of its own.
    pub fn repo_role(self) -> Role {
        match self {
            OrgRole::Member => Role::Write,
            OrgRole::Admin | OrgRole::Owner => Role::Admin,
        }
    }
}

fn role_name(r: Option<OrgRole>) -> &'static str {
    r.map(OrgRole::as_str).unwrap_or("none")
}

/// What an organization pays for: free has a cap on proof minutes a month, the others none.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Free,
    Team,
    Enterprise,
}

impl Plan {
    pub fn parse(s: &str) -> Result<Plan> {
        match s.trim() {
            "free" => Ok(Plan::Free),
            "team" => Ok(Plan::Team),
            "enterprise" => Ok(Plan::Enterprise),
            other => Err(anyhow!("{other:?} is no plan: free ({} proof minutes a month), team or enterprise (no cap)", free_minutes())),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Team => "team",
            Plan::Enterprise => "enterprise",
        }
    }

    /// The proof minutes a month it allows; None, as many as it takes.
    pub fn cap(self) -> Option<f64> {
        (self == Plan::Free).then(free_minutes)
    }
}

/// The free plan's proof minutes a month: `AVP_FREE_MINUTES`, 2000 unless it says otherwise.
pub fn free_minutes() -> f64 {
    std::env::var("AVP_FREE_MINUTES").ok().and_then(|v| v.trim().parse().ok()).filter(|m: &f64| *m >= 0.0).unwrap_or(2000.0)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Org {
    pub name: String,
    pub plan: Plan,
    pub created_at: i64,
    pub created_by: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Invite {
    pub id: String,
    pub org: String,
    /// Who it asks: a person by name, or an email address whoever signs in with it may answer.
    pub person: Option<String>,
    pub email: Option<String>,
    pub role: OrgRole,
    pub by: String,
    pub at: i64,
}

fn invite_row(r: &rusqlite::Row) -> rusqlite::Result<Invite> {
    Ok(Invite { id: r.get(0)?, org: r.get(1)?, person: r.get(2)?, email: r.get(3)?, role: OrgRole::parse(&r.get::<_, String>(4)?).ok().flatten().unwrap_or(OrgRole::Member), by: r.get(5)?, at: r.get(6)? })
}
const INVITE_COLS: &str = "id, org, person, email, role, by, at";

// ---- organizations and members ----

pub fn org(db: &Db, name: &str) -> Result<Option<Org>> {
    Ok(db
        .conn
        .query_row("SELECT name, plan, created_at, created_by FROM orgs WHERE name = ?1", params![name], |r| {
            Ok(Org { name: r.get(0)?, plan: Plan::parse(&r.get::<_, String>(1)?).unwrap_or(Plan::Free), created_at: r.get(2)?, created_by: r.get(3)? })
        })
        .optional()?)
}

pub fn orgs(db: &Db) -> Result<Vec<Org>> {
    let mut st = db.conn.prepare("SELECT name FROM orgs ORDER BY name")?;
    let names = st.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    names.iter().filter_map(|n| org(db, n).transpose()).collect()
}

pub fn member(db: &Db, org: &str, person: &str) -> Result<Option<OrgRole>> {
    let r: Option<String> = db.conn.query_row("SELECT role FROM members WHERE org = ?1 AND person = ?2", params![org, person], |r| r.get(0)).optional()?;
    Ok(r.and_then(|r| OrgRole::parse(&r).ok().flatten()))
}

pub fn members(db: &Db, org: &str) -> Result<Vec<(String, OrgRole)>> {
    let mut st = db.conn.prepare("SELECT person, role FROM members WHERE org = ?1 ORDER BY person")?;
    let rows = st.query_map(params![org], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter_map(|(p, r)| Some((p, OrgRole::parse(&r).ok()??))).collect())
}

/// The organizations a person belongs to, with their role in each.
pub fn orgs_of(db: &Db, person: &str) -> Result<Vec<(Org, OrgRole)>> {
    let mut st = db.conn.prepare("SELECT org, role FROM members WHERE person = ?1 ORDER BY org")?;
    let rows = st.query_map(params![person], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut out = vec![];
    for (o, r) in rows {
        if let (Some(o), Ok(Some(r))) = (org(db, &o)?, OrgRole::parse(&r)) {
            out.push((o, r));
        }
    }
    Ok(out)
}

/// An organization the asker may see: a member's, or any for a server admin. One they are not in
/// answers as one that does not exist.
fn visible(db: &Db, name: &str, who: &Who) -> Result<(Org, Option<OrgRole>)> {
    let none = || anyhow!("no org {name} (or none you are a member of: `avp org list`)");
    let o = org(db, name)?.ok_or_else(none)?;
    let role = match who {
        Who::Person(u) => member(db, name, &u.name)?,
        _ => None,
    };
    anyhow::ensure!(role.is_some() || who.root(), none());
    Ok((o, role))
}

/// An organization the asker may run: its admins' and owners', a server admin's.
fn administered(db: &Db, name: &str, who: &Who) -> Result<(Org, Option<OrgRole>)> {
    let (o, role) = visible(db, name, who)?;
    anyhow::ensure!(who.root() || role >= Some(OrgRole::Admin), Forbidden(format!("{} is a member of {name}: its admins and owners invite, give roles and bring repos in", who.name())));
    Ok((o, role))
}

/// A new organization, with its creator as its owner, on the free plan. Its name is no person's:
/// a repo's space is named by one or the other.
pub fn create(db: &Db, name: &str, by: &str) -> Result<Org> {
    needs_cloud()?;
    auth::valid_name(name, "an organization's")?;
    anyhow::ensure!(org(db, name)?.is_none() && auth::user(db, name)?.is_none(), "{name} is taken, by an organization or a person: choose another name");
    let o = Org { name: name.into(), plan: Plan::Free, created_at: now_ms(), created_by: by.into() };
    db.conn.execute("INSERT INTO orgs (name, plan, created_at, created_by) VALUES (?1, ?2, ?3, ?4)", params![o.name, o.plan.as_str(), o.created_at, o.created_by])?;
    db.conn.execute("INSERT INTO members (org, person, role, since) VALUES (?1, ?2, 'owner', ?3)", params![o.name, by, o.created_at])?;
    db.emit(SERVER, "org.created", None, None, json!({ "org": name, "plan": o.plan, "by": by }))?;
    db.emit(SERVER, "org.member", None, None, json!({ "org": name, "person": by, "role": "owner", "was": "none", "by": by }))?;
    Ok(o)
}

/// Changes a member's role, or takes them out (None). Only an owner makes or unmakes an owner, and
/// an organization keeps one owner at least. A member may leave by themselves.
pub fn set_member(db: &Db, name: &str, person: &str, role: Option<OrgRole>, who: &Who) -> Result<Option<OrgRole>> {
    needs_cloud()?;
    let leaving = role.is_none() && who.person() == Some(person);
    let (_, mine) = if leaving { visible(db, name, who)? } else { administered(db, name, who)? };
    let was = member(db, name, person)?;
    anyhow::ensure!(was.is_some(), "{person} is no member of {name}: `avp org invite {name} {person}` asks them");
    if role == Some(OrgRole::Owner) || was == Some(OrgRole::Owner) {
        anyhow::ensure!(who.root() || mine == Some(OrgRole::Owner) || leaving, Forbidden(format!("only an owner of {name} makes or unmakes an owner")));
    }
    let owners = members(db, name)?.iter().filter(|(_, r)| *r == OrgRole::Owner).count();
    anyhow::ensure!(!(was == Some(OrgRole::Owner) && role != Some(OrgRole::Owner) && owners == 1), "{person} is {name}'s last owner: make another member owner first (`avp org role {name} <person> owner`)");
    match role {
        Some(r) => db.conn.execute("UPDATE members SET role = ?3 WHERE org = ?1 AND person = ?2", params![name, person, r.as_str()])?,
        None => db.conn.execute("DELETE FROM members WHERE org = ?1 AND person = ?2", params![name, person])?,
    };
    if was != role {
        db.emit(SERVER, "org.member", None, None, json!({ "org": name, "person": person, "role": role_name(role), "was": role_name(was), "by": who.name() }))?;
    }
    Ok(was)
}

/// Asks a person, by name or by email, to join with a role: they accept or decline it.
pub fn invite(db: &Db, name: &str, to: &str, role: OrgRole, who: &Who) -> Result<Invite> {
    needs_cloud()?;
    let (_, mine) = administered(db, name, who)?;
    anyhow::ensure!(role != OrgRole::Owner || who.root() || mine == Some(OrgRole::Owner), Forbidden(format!("only an owner of {name} invites an owner")));
    let to = to.trim();
    let (person, email) = if to.contains('@') {
        (None, Some(to.to_lowercase()))
    } else {
        anyhow::ensure!(auth::user(db, to)?.is_some(), "no user {to}: invite them by email, they answer once they signed up");
        anyhow::ensure!(member(db, name, to)?.is_none(), "{to} is already a member of {name}: `avp org role {name} {to} <role>` changes their role");
        (Some(to.to_string()), None)
    };
    let i = Invite { id: new_id("inv"), org: name.into(), person, email, role, by: who.name(), at: now_ms() };
    db.conn.execute(&format!("INSERT INTO invites ({INVITE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)"), params![i.id, i.org, i.person, i.email, i.role.as_str(), i.by, i.at])?;
    db.emit(SERVER, "org.invited", None, None, json!({ "org": name, "id": i.id, "person": i.person, "email": i.email, "role": role, "by": i.by }))?;
    Ok(i)
}

/// The invitations a person may answer: to their name, or to their email.
pub fn invites_for(db: &Db, person: &str) -> Result<Vec<Invite>> {
    let email = auth::user(db, person)?.and_then(|u| u.email).map(|e| e.to_lowercase());
    let mut st = db.conn.prepare(&format!("SELECT {INVITE_COLS} FROM invites WHERE answered_at IS NULL AND (person = ?1 OR (?2 IS NOT NULL AND email = ?2)) ORDER BY at"))?;
    let rows = st.query_map(params![person, email], invite_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The invitations of an organization no one answered yet.
pub fn pending(db: &Db, org: &str) -> Result<Vec<Invite>> {
    let mut st = db.conn.prepare(&format!("SELECT {INVITE_COLS} FROM invites WHERE answered_at IS NULL AND org = ?1 ORDER BY at"))?;
    let rows = st.query_map(params![org], invite_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A person accepts an invitation (they become a member, with its role) or declines it.
pub fn answer(db: &Db, id: &str, person: &str, accept: bool) -> Result<Invite> {
    needs_cloud()?;
    let i = invites_for(db, person)?.into_iter().find(|i| i.id == id).ok_or_else(|| anyhow!("no invitation {id} for {person}: `avp org invites` lists yours"))?;
    let now = now_ms();
    db.conn.execute("UPDATE invites SET answered_at = ?2, accepted = ?3 WHERE id = ?1", params![id, now, accept as i64])?;
    if accept {
        // Already a member (another invitation answered first): the higher role stays.
        let role = member(db, &i.org, person)?.map_or(i.role, |r| r.max(i.role));
        db.conn.execute(
            "INSERT INTO members (org, person, role, since) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (org, person) DO UPDATE SET role = excluded.role",
            params![i.org, person, role.as_str(), now],
        )?;
    }
    db.emit(SERVER, if accept { "org.joined" } else { "org.declined" }, None, None, json!({ "org": i.org, "id": id, "person": person, "role": i.role, "by": person }))?;
    Ok(i)
}

/// A server admin's: what the organization pays for.
pub fn set_plan(db: &Db, name: &str, plan: Plan, by: &str) -> Result<Org> {
    needs_cloud()?;
    let mut o = org(db, name)?.ok_or_else(|| anyhow!("no org {name}: `avp org list`"))?;
    if o.plan != plan {
        db.conn.execute("UPDATE orgs SET plan = ?2 WHERE name = ?1", params![name, plan.as_str()])?;
        db.emit(SERVER, "org.plan", None, None, json!({ "org": name, "plan": plan, "was": o.plan, "by": by }))?;
        forget_usage();
        o.plan = plan;
    }
    Ok(o)
}

/// Brings a repo into an organization: an admin of both does it. Its members then reach it by their
/// org role; the roles given on the repo itself stay.
pub fn bring_repo(db: &Db, name: &str, repo: &str, who: &Who) -> Result<Repo> {
    needs_cloud()?;
    administered(db, name, who)?;
    // A name alone is the person's own repo first: another space may have one of that name.
    let own = who.person().filter(|_| !repo.contains('/')).and_then(|p| db.repo(&format!("{p}/{repo}")).ok());
    let mut r = match own {
        Some(r) => r,
        None => db.repo(repo)?,
    };
    if let Who::Person(u) = who {
        anyhow::ensure!(u.admin || auth::role(db, &r.id, &u.name)? == Some(Role::Admin), Forbidden(format!("{} is no admin of {}: an admin of the repo brings it into an organization", u.name, r.name)));
    }
    if r.org.as_deref() != Some(name) {
        anyhow::ensure!(db.repo_in(Some(name), &r.name)?.is_none(), "{name}/{} already exists: rename one of them first, or bring the other repo", r.name);
        let was = space_of(db, &r.id)?;
        db.conn.execute("UPDATE repos SET org = ?2 WHERE id = ?1", params![r.id, name])?;
        db.emit(SERVER, "org.repo", None, None, json!({ "org": name, "repo": r.name, "was": was, "by": who.name() }))?;
        forget_usage();
        r.org = Some(name.into());
        r.space = Some(name.into());
    }
    Ok(r)
}

/// A new repo in its space: `<space>/<name>`, where the space is the person's own or an
/// organization they administer, or `<name>` alone, in the person's own. The server's key names
/// any space there is, or none.
pub fn create_repo(e: &Engine, name: &str, policy: crate::model::LeasePolicy, who: &Who) -> Result<Repo> {
    let (space, name) = match name.trim().split_once('/') {
        Some((s, n)) => (Some(s), n),
        None => (who.person(), name.trim()),
    };
    auth::valid_name(name, "a repo's")?;
    let (org, owner) = {
        let db = e.db.lock().unwrap();
        match space {
            None => (None, None),
            Some(s) if who.person() == Some(s) => (None, Some(s)),
            Some(s) if org(&db, s)?.is_some() => {
                administered(&db, s, who)?;
                (Some(s), None)
            }
            Some(s) if who.root() && auth::user(&db, s)?.is_some() => (None, Some(s)),
            Some(s) => anyhow::bail!("no space {s} of yours: a repo goes in your own ({}) or in an organization you administer (`avp org list`)", who.person().unwrap_or("none")),
        }
    };
    let r = e.create_repo_in(name, policy, org, owner)?;
    if let Some(person) = who.person() {
        auth::set_role(&e.db.lock().unwrap(), &r, person, Some(Role::Admin), person)?;
    }
    forget_usage();
    Ok(r)
}

/// Repos from before spaces that no organization holds: the person who created them (the first
/// given a role on them) is their space. A team server's stay in none.
pub fn settle_spaces(db: &Db) -> Result<usize> {
    if !on() {
        return Ok(0);
    }
    Ok(db.conn.execute(
        "UPDATE repos SET owner = (SELECT json_extract(data, '$.person') FROM events WHERE events.repo = repos.id AND kind = 'role.set' ORDER BY id LIMIT 1) WHERE org IS NULL AND owner IS NULL",
        [],
    )?)
}

/// The organization's events, or with `org` none, every organization's: oldest first.
pub fn events(db: &Db, org: Option<&str>, since: i64, limit: i64) -> Result<Vec<crate::model::Event>> {
    let mut st = db.conn.prepare(
        "SELECT id, at, kind, task, path, data FROM events WHERE repo = ?1 AND id > ?2 AND kind LIKE 'org.%' \
         AND (?3 IS NULL OR json_extract(data, '$.org') = ?3) ORDER BY id LIMIT ?4",
    )?;
    let rows = st.query_map(params![SERVER, since, org, limit.clamp(1, 10_000)], |r| {
        Ok(crate::model::Event { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, task: r.get(3)?, path: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null) })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---- spaces and their minutes ----

/// Whose minutes a repo's proofs are: its organization's, or the person's whose own it is. A repo
/// of a team server, or one the server's key made, is nobody's: it is not metered.
pub fn space_of(db: &Db, repo: &str) -> Result<Option<String>> {
    Ok(db.repo(repo).ok().and_then(|r| r.space))
}

/// The repos of an organization, or of a person's own space.
fn repos_of(db: &Db, space: &str) -> Result<Vec<(String, String)>> {
    let mut st = db.conn.prepare("SELECT id, name FROM repos WHERE org = ?1 OR (org IS NULL AND owner = ?1) ORDER BY name")?;
    let rows = st.query_map(params![space], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The calendar month (UTC) `now` falls in: its name (`2026-11`), its first and its last instant.
pub fn month(now: i64) -> (String, i64, i64) {
    let name = rfc3339(now)[..7].to_string();
    let (y, m): (i64, i64) = (name[..4].parse().unwrap_or(1970), name[5..7].parse().unwrap_or(1));
    let start = parse_time(&format!("{name}-01")).unwrap_or(0);
    let next = if m == 12 { format!("{}-01-01", y + 1) } else { format!("{y}-{:02}-01", m + 1) };
    (name, start, parse_time(&next).unwrap_or(i64::MAX))
}

/// A space's month: the worker time of its repos' proofs and full suites, against its plan's cap.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Usage {
    pub space: String,
    /// `org`, or `person` for a person's own repos.
    pub kind: String,
    pub plan: Plan,
    pub month: String,
    pub minutes: f64,
    pub proof_minutes: f64,
    pub full_minutes: f64,
    /// None: no cap on this plan.
    pub cap: Option<f64>,
    /// Past the cap: a new proof is refused until the month ends or the plan changes.
    pub over: bool,
    /// When the next month starts (RFC3339, UTC).
    pub resets_at: String,
    /// Members of the organization, 1 for a person's own space.
    pub seats: usize,
    pub repos: Vec<String>,
    /// Past the cap: what a refused proof says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
}

pub fn usage(db: &Db, space: &str, now: i64) -> Result<Usage> {
    let (name, since, until) = month(now);
    let o = org(db, space)?;
    let plan = o.as_ref().map(|o| o.plan).unwrap_or(Plan::Free);
    let repos = repos_of(db, space)?;
    let (mut proofs, mut full) = (0.0, 0.0);
    for (id, _) in &repos {
        let m = super::stats::machine_minutes(db, id, since, until)?;
        (proofs, full) = (proofs + m.proofs, full + m.full_suites);
    }
    let minutes = tenth(proofs + full);
    let cap = plan.cap();
    let mut u = Usage {
        space: space.into(),
        kind: if o.is_some() { "org" } else { "person" }.into(),
        plan,
        month: name,
        minutes,
        proof_minutes: tenth(proofs),
        full_minutes: tenth(full),
        cap,
        over: cap.is_some_and(|c| minutes >= c),
        resets_at: rfc3339(until),
        seats: if o.is_some() { members(db, space)?.len() } else { 1 },
        repos: repos.into_iter().map(|(_, n)| n).collect(),
        said: None,
    };
    if u.over {
        u.said = Some(over_text(&u));
    }
    Ok(u)
}

/// What a refused proof says: the usage, the cap, and how to go further.
pub fn over_text(u: &Usage) -> String {
    let more = if u.kind == "org" {
        format!("a server admin moves {} to the team plan (`avp org plan {} team`)", u.space, u.space)
    } else {
        "bring the repo into an organization on the team plan (`avp org repo <org> <repo>`)".to_string()
    };
    format!(
        "{} used {} of its {} proof minutes this month ({} plan): a new proof waits until {} (UTC), when the month starts again, or {more}. `avp org usage` shows the count",
        u.space,
        u.minutes,
        u.cap.unwrap_or_default(),
        u.plan.as_str(),
        &u.resets_at[..10]
    )
}

/// The minutes of each space as last read, with the newest proof of the server then: every land and
/// check asks, and a month of proofs is read again only once a proof was made since.
static READ: std::sync::Mutex<Option<std::collections::HashMap<String, (i64, Usage)>>> = std::sync::Mutex::new(None);

/// A plan changed, a repo changed space: every space is read again.
fn forget_usage() {
    *READ.lock().unwrap() = None;
}

/// Whether a new proof of this repo may run: Err says why not, when its space is past its cap.
pub fn may_prove(e: &Engine, repo: &str) -> Result<std::result::Result<(), String>> {
    if !on() {
        return Ok(Ok(()));
    }
    let now = now_ms();
    let db = e.db.lock().unwrap();
    let Some(space) = space_of(&db, repo)? else { return Ok(Ok(())) };
    let newest: i64 = db.conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM proofs", [], |r| r.get(0))?;
    let month_now = month(now).0;
    let cached = READ.lock().unwrap().as_ref().and_then(|m| m.get(&space)).filter(|(at, u)| *at == newest && u.month == month_now).map(|(_, u)| u.clone());
    let u = match cached {
        Some(u) => u,
        None => {
            let u = usage(&db, &space, now)?;
            READ.lock().unwrap().get_or_insert_with(Default::default).insert(space.clone(), (newest, u.clone()));
            u
        }
    };
    Ok(if u.over { Err(over_text(&u)) } else { Ok(()) })
}

// ---- the routes ----

type S = State<Engine>;
use super::auth::run;

pub fn routes() -> Router<Engine> {
    Router::new()
        .route("/orgs", get(list).post(create_route))
        .route("/orgs/{org}", get(show))
        .route("/orgs/{org}/members", post(member_route))
        .route("/orgs/{org}/invites", post(invite_route))
        .route("/orgs/{org}/repos", post(repo_route))
        .route("/orgs/{org}/plan", post(plan_route))
        .route("/orgs/{org}/usage", get(usage_route))
        .route("/orgs/{org}/events", get(events_route))
        .route("/invites", get(my_invites))
        .route("/invites/{id}", post(answer_route))
        .route("/repos/{repo}/usage", get(repo_usage))
}

/// The organizations the asker belongs to (a server admin: all of them).
async fn list(State(e): S, Extension(who): Extension<Who>) -> Response {
    run(move || {
        needs_cloud()?;
        let db = e.db.lock().unwrap();
        let mine: Vec<(Org, Option<OrgRole>)> = match &who {
            Who::Person(u) if !u.admin => orgs_of(&db, &u.name)?.into_iter().map(|(o, r)| (o, Some(r))).collect(),
            _ => {
                let me = who.person().map(String::from);
                let mut all = vec![];
                for o in orgs(&db)? {
                    let r = match &me { Some(p) => member(&db, &o.name, p)?, None => None };
                    all.push((o, r));
                }
                all
            }
        };
        let mut out = vec![];
        for (o, role) in mine {
            let seats = members(&db, &o.name)?.len();
            out.push(json!({ "name": o.name, "plan": o.plan, "role": role, "seats": seats, "created_at": o.created_at }));
        }
        Ok(out)
    })
    .await
}

#[derive(Deserialize)]
struct Create {
    name: String,
}

async fn create_route(State(e): S, Extension(who): Extension<Who>, Json(b): Json<Create>) -> Response {
    run(move || {
        let Who::Person(u) = &who else { anyhow::bail!("an organization has a person for owner: create it with a person's token (AVP_TOKEN)") };
        create(&e.db.lock().unwrap(), b.name.trim(), &u.name)
    })
    .await
}

/// An organization as its members read it: plan, members, repos, and for its admins the
/// invitations no one answered yet.
async fn show(State(e): S, Extension(who): Extension<Who>, P(name): P<String>) -> Response {
    run(move || {
        needs_cloud()?;
        let db = e.db.lock().unwrap();
        let (o, role) = visible(&db, &name, &who)?;
        let members: Vec<_> = members(&db, &name)?.into_iter().map(|(p, r)| json!({ "person": p, "role": r })).collect();
        let invites = if who.root() || role >= Some(OrgRole::Admin) { pending(&db, &name)? } else { vec![] };
        let repos: Vec<String> = repos_of(&db, &name)?.into_iter().map(|(_, n)| n).collect();
        Ok(json!({ "name": o.name, "plan": o.plan, "role": role, "seats": members.len(), "members": members, "repos": repos, "invites": invites, "created_at": o.created_at, "created_by": o.created_by }))
    })
    .await
}

#[derive(Deserialize)]
struct Member {
    person: String,
    role: String,
}

async fn member_route(State(e): S, Extension(who): Extension<Who>, P(name): P<String>, Json(b): Json<Member>) -> Response {
    run(move || {
        let role = OrgRole::parse(&b.role)?;
        let was = set_member(&e.db.lock().unwrap(), &name, b.person.trim(), role, &who)?;
        Ok(json!({ "org": name, "person": b.person.trim(), "role": role_name(role), "was": role_name(was) }))
    })
    .await
}

#[derive(Deserialize)]
struct InviteBody {
    /// A person's name, or an email.
    to: String,
    role: Option<String>,
}

async fn invite_route(State(e): S, Extension(who): Extension<Who>, P(name): P<String>, Json(b): Json<InviteBody>) -> Response {
    run(move || {
        let role = OrgRole::parse(b.role.as_deref().unwrap_or("member"))?.ok_or_else(|| anyhow!("an invitation gives a role: member, admin or owner"))?;
        invite(&e.db.lock().unwrap(), &name, &b.to, role, &who)
    })
    .await
}

#[derive(Deserialize)]
struct RepoBody {
    repo: String,
}

async fn repo_route(State(e): S, Extension(who): Extension<Who>, P(name): P<String>, Json(b): Json<RepoBody>) -> Response {
    run(move || bring_repo(&e.db.lock().unwrap(), &name, b.repo.trim(), &who)).await
}

#[derive(Deserialize)]
struct PlanBody {
    plan: String,
}

async fn plan_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(name): P<String>, Json(b): Json<PlanBody>) -> Response {
    run(move || {
        let plan = Plan::parse(&b.plan)?;
        set_plan(&e.db.lock().unwrap(), &name, plan, &who.by(&headers))
    })
    .await
}

/// A space's month: an organization's, for its members, or the asker's own (their name).
async fn usage_route(State(e): S, Extension(who): Extension<Who>, P(name): P<String>) -> Response {
    run(move || {
        needs_cloud()?;
        let db = e.db.lock().unwrap();
        if who.person() != Some(name.as_str()) {
            visible(&db, &name, &who)?;
        }
        usage(&db, &name, now_ms())
    })
    .await
}

/// The month of the space a repo belongs to, for whoever reads the repo: what `avp status` and
/// `avp stats` say of the cap. A repo of no space answers `{"space": null}`.
async fn repo_usage(State(e): S, P(repo): P<String>) -> Response {
    run(move || {
        needs_cloud()?;
        let db = e.db.lock().unwrap();
        let r = db.repo(&repo)?;
        Ok(match space_of(&db, &r.id)? {
            Some(space) => serde_json::to_value(usage(&db, &space, now_ms())?)?,
            None => json!({ "space": null }),
        })
    })
    .await
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn events_route(State(e): S, Extension(who): Extension<Who>, P(name): P<String>, Query(q): Query<EventsQuery>) -> Response {
    run(move || {
        needs_cloud()?;
        let db = e.db.lock().unwrap();
        administered(&db, &name, &who)?;
        events(&db, Some(&name), q.since.unwrap_or(0), q.limit.unwrap_or(1000))
    })
    .await
}

async fn my_invites(State(e): S, Extension(who): Extension<Who>) -> Response {
    run(move || {
        needs_cloud()?;
        let Some(me) = who.person() else { return Ok(vec![]) };
        invites_for(&e.db.lock().unwrap(), me)
    })
    .await
}

#[derive(Deserialize)]
struct Answer {
    accept: bool,
}

async fn answer_route(State(e): S, Extension(who): Extension<Who>, P(id): P<String>, Json(b): Json<Answer>) -> Response {
    run(move || {
        let Some(me) = who.person() else { anyhow::bail!("an invitation is a person's to answer: with their token (AVP_TOKEN)") };
        let i = answer(&e.db.lock().unwrap(), &id, me, b.accept)?;
        Ok(json!({ "org": i.org, "role": i.role, "accepted": b.accept }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_month_runs_from_its_first_day_to_the_next_months() {
        let (name, since, until) = month(parse_time("2026-12-31T23:59:00Z").unwrap());
        assert_eq!((name.as_str(), since, until), ("2026-12", parse_time("2026-12-01").unwrap(), parse_time("2027-01-01").unwrap()));
        let (name, _, until) = month(parse_time("2026-02-10").unwrap());
        assert_eq!((name.as_str(), rfc3339(until)[..10].to_string()), ("2026-02", "2026-03-01".to_string()));
    }

    #[test]
    fn an_org_role_gives_its_repos_role() {
        assert_eq!(OrgRole::Member.repo_role(), Role::Write);
        assert_eq!(OrgRole::Admin.repo_role(), Role::Admin);
        assert_eq!(OrgRole::Owner.repo_role(), Role::Admin);
        assert!(OrgRole::parse("boss").is_err() && OrgRole::parse("none").unwrap().is_none());
    }
}
