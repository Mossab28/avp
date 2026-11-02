//! People, tokens and rights on a team server. A server without any user works as it always has:
//! its one token (`AVP_TOKEN`), a proxy in front that says who is asking, or nothing on 127.0.0.1.
//! The first `avp user add` turns accounts on: from then on a request carries a person's token, the
//! person on what it does is that token's (whatever `AVP_PERSON` says), and the person's role on
//! the repo says what they may do. `AVP_TOKEN` stays the server's own key, for its machines
//! (workers, mirrors), with every right, as a request from the server's own machine has.

use crate::db::Db;
use crate::engine::Engine;
use crate::model::{new_id, now_ms, Event, LeasePolicy, NewToken, Repo, TokenInfo, User};
use anyhow::{anyhow, Result};
use axum::extract::{Path as P, Query, State};
use axum::http::{Extensions, HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use rusqlite::{params, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Where the events of people and tokens go: they belong to the server, not to one repo. A role's
/// goes to its repo.
const SERVER: &str = "";
/// AVP Live's cookie: a session of its own, revoked by signing out, never the person's token.
const SESSION: &str = "avp_session";
const SESSION_DAYS: i64 = 14;
/// The state a GitHub sign-in carries there and back, against a forged return.
const OAUTH: &str = "avp_oauth";

/// What a person may do on a repo, each role holding the ones below it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Read,
    Write,
    Admin,
}

impl Role {
    /// A role by name; `none` is no role.
    pub fn parse(s: &str) -> Result<Option<Role>> {
        match s.trim() {
            "read" => Ok(Some(Role::Read)),
            "write" => Ok(Some(Role::Write)),
            "admin" => Ok(Some(Role::Admin)),
            "none" => Ok(None),
            other => Err(anyhow!("{other:?} is no role: read (the line, AVP Live), write (open and land tasks), admin (guards, landing rule, environments, secrets, roles), or none")),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Admin => "admin",
        }
    }

    fn does(self) -> &'static str {
        match self {
            Role::Read => "read the line",
            Role::Write => "open and land tasks",
            Role::Admin => "change guards, the landing rule, environments, secrets and roles",
        }
    }
}

fn user_row(r: &Row) -> rusqlite::Result<User> {
    Ok(User { name: r.get(0)?, email: r.get(1)?, github: r.get(2)?, admin: r.get::<_, i64>(3)? != 0, created_at: r.get(4)?, created_by: r.get(5)? })
}
const USER_COLS: &str = "name, email, github, admin, created_at, created_by";

fn token_row(r: &Row) -> rusqlite::Result<TokenInfo> {
    Ok(TokenInfo { id: r.get(0)?, person: r.get(1)?, name: r.get(2)?, kind: r.get(3)?, created_at: r.get(4)?, expires_at: r.get(5)?, revoked_at: r.get(6)?, used_at: r.get(7)? })
}
const TOKEN_COLS: &str = "id, person, name, kind, created_at, expires_at, revoked_at, used_at";

/// Who a request comes from, as the gate found it (a request extension).
#[derive(Clone, Debug)]
pub enum Who {
    /// A server without accounts: whoever got past what guards it, as before accounts.
    Anyone,
    /// The server's own key, with the name given with it, or a request from its own machine
    /// (None): every right.
    Server(Option<String>),
    /// A person, by a token of theirs.
    Person(User),
}

impl Who {
    /// Every right: the server itself, an admin of it, or anyone on a server without accounts.
    pub fn root(&self) -> bool {
        match self {
            Who::Person(u) => u.admin,
            _ => true,
        }
    }

    pub(crate) fn person(&self) -> Option<&str> {
        match self {
            Who::Person(u) => Some(&u.name),
            _ => None,
        }
    }

    /// The name a change of an organization is recorded under.
    pub(crate) fn name(&self) -> String {
        self.by(&HeaderMap::new())
    }

    /// The name a change of people, tokens or roles is recorded under.
    pub(crate) fn by(&self, headers: &HeaderMap) -> String {
        match self {
            Who::Person(u) => u.name.clone(),
            Who::Server(Some(p)) => p.clone(),
            Who::Server(None) => "server".into(),
            Who::Anyone => headers.get("x-avp-user").and_then(|v| v.to_str().ok()).map(str::trim).filter(|s| !s.is_empty()).unwrap_or("someone").into(),
        }
    }

    fn via(&self) -> &'static str {
        match self {
            Who::Anyone => "open",
            Who::Server(Some(_)) => "key",
            Who::Server(None) => "machine",
            Who::Person(_) => "token",
        }
    }
}

/// A refusal that is about rights (403), not a request gone wrong.
#[derive(Debug)]
pub(crate) struct Forbidden(pub(crate) String);

impl std::fmt::Display for Forbidden {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Forbidden {}

// ---- people ----

/// Accounts are on once the server has a person.
pub fn on(db: &Db) -> Result<bool> {
    Ok(db.conn.query_row("SELECT EXISTS (SELECT 1 FROM users)", [], |r| r.get(0))?)
}

pub fn user(db: &Db, name: &str) -> Result<Option<User>> {
    Ok(db.conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE name = ?1"), params![name], user_row).optional()?)
}

pub fn users(db: &Db) -> Result<Vec<User>> {
    let mut st = db.conn.prepare(&format!("SELECT {USER_COLS} FROM users ORDER BY created_at"))?;
    let rows = st.query_map([], user_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// The person a GitHub login signs in as.
pub fn user_of_github(db: &Db, login: &str) -> Result<Option<User>> {
    Ok(db.conn.query_row(&format!("SELECT {USER_COLS} FROM users WHERE github = ?1"), params![github_login(login)], user_row).optional()?)
}

fn github_login(s: &str) -> String {
    s.trim().trim_start_matches('@').to_lowercase()
}

/// Adds a person, with a first token for them, shown once. The first person turns accounts on and
/// is an admin of the server: nobody else could give rights then.
pub fn add_user(db: &Db, name: &str, email: Option<&str>, github: Option<&str>, admin: bool, by: &str) -> Result<(User, NewToken)> {
    add_person(db, name, email, github, admin || !on(db)?, by)
}

/// A name of a person or an organization: letters, digits, `.`, `_` and `-`.
pub(crate) fn valid_name(name: &str, whose: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 64 && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !name.starts_with(['.', '-']),
        "{name:?}: {whose} name is letters, digits, `.`, `_` and `-`, 64 at most"
    );
    Ok(())
}

/// A person whatever came before: one who signs up is never an admin, even the first.
fn add_person(db: &Db, name: &str, email: Option<&str>, github: Option<&str>, admin: bool, by: &str) -> Result<(User, NewToken)> {
    valid_name(name, "a person's")?;
    anyhow::ensure!(user(db, name)?.is_none(), "{name} is already a person of this server: `avp user list`");
    // A repo's space is named by its organization or its person: one name, one of them.
    anyhow::ensure!(super::orgs::org(db, name)?.is_none(), "{name} is an organization's name: choose another");
    let github = github.map(github_login).filter(|g| !g.is_empty());
    if let Some(g) = &github {
        if let Some(other) = user_of_github(db, g)? {
            anyhow::bail!("the GitHub login {g} already signs in {}", other.name);
        }
    }
    let u = User { name: name.into(), email: email.map(str::trim).filter(|e| !e.is_empty()).map(String::from), github, admin, created_at: now_ms(), created_by: by.into() };
    db.conn.execute(
        &format!("INSERT INTO users ({USER_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"),
        params![u.name, u.email, u.github, u.admin as i64, u.created_at, u.created_by],
    )?;
    db.emit(SERVER, "user.added", None, None, json!({ "name": u.name, "email": u.email, "github": u.github, "admin": u.admin, "by": by }))?;
    let token = create_token(db, name, "first", "token", None, by)?;
    Ok((u, token))
}

/// Takes a person off the server: their tokens stop working, their roles go.
pub fn remove_user(db: &Db, name: &str, by: &str) -> Result<()> {
    anyhow::ensure!(user(db, name)?.is_some(), "no user {name}: `avp user list`");
    let now = now_ms();
    let revoked = db.conn.execute("UPDATE tokens SET revoked_at = ?2 WHERE person = ?1 AND revoked_at IS NULL", params![name, now])?;
    db.conn.execute("DELETE FROM roles WHERE person = ?1", params![name])?;
    db.conn.execute("DELETE FROM members WHERE person = ?1", params![name])?;
    db.conn.execute("DELETE FROM passwords WHERE person = ?1", params![name])?;
    db.conn.execute("DELETE FROM users WHERE name = ?1", params![name])?;
    db.emit(SERVER, "user.removed", None, None, json!({ "name": name, "tokens_revoked": revoked, "by": by }))?;
    Ok(())
}

pub fn set_admin(db: &Db, name: &str, admin: bool, by: &str) -> Result<User> {
    let mut u = user(db, name)?.ok_or_else(|| anyhow!("no user {name}: `avp user list`"))?;
    if u.admin != admin {
        db.conn.execute("UPDATE users SET admin = ?2 WHERE name = ?1", params![name, admin as i64])?;
        db.emit(SERVER, "user.admin", None, None, json!({ "name": name, "admin": admin, "by": by }))?;
        u.admin = admin;
    }
    Ok(u)
}

// ---- tokens ----

/// What the server keeps of a token. A token is 32 random bytes, so a fast hash is enough: there is
/// nothing to guess from it, no password to try words against.
pub fn hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

fn fresh() -> Result<String> {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Generate, Key};
    let bytes: [u8; 32] = Key::<chacha20poly1305::XChaCha20Poly1305>::try_generate().map_err(|e| anyhow!("the system's random generator: {e}"))?.into();
    Ok(format!("avp_{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)))
}

pub fn create_token(db: &Db, person: &str, name: &str, kind: &str, expires_at: Option<i64>, by: &str) -> Result<NewToken> {
    let token = fresh()?;
    let info = TokenInfo { id: new_id("tok"), person: person.into(), name: name.into(), kind: kind.into(), created_at: now_ms(), expires_at, revoked_at: None, used_at: None };
    db.conn.execute(
        "INSERT INTO tokens (id, hash, person, name, kind, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![info.id, hash(&token), info.person, info.name, info.kind, info.created_at, info.expires_at],
    )?;
    db.emit(SERVER, "token.created", None, None, json!({ "id": info.id, "person": person, "name": name, "kind": kind, "expires_at": expires_at, "by": by }))?;
    Ok(NewToken { token, info })
}

/// What a presented token is.
#[derive(Debug)]
pub enum Checked {
    Person(User),
    Unknown,
    Revoked,
    Expired(i64),
}

pub fn check_token(db: &Db, token: &str) -> Result<Checked> {
    let Some(t) = db.conn.query_row(&format!("SELECT {TOKEN_COLS} FROM tokens WHERE hash = ?1"), params![hash(token)], token_row).optional()? else { return Ok(Checked::Unknown) };
    let now = now_ms();
    if t.revoked_at.is_some() {
        return Ok(Checked::Revoked);
    }
    if let Some(at) = t.expires_at.filter(|at| *at <= now) {
        return Ok(Checked::Expired(at));
    }
    let Some(u) = user(db, &t.person)? else { return Ok(Checked::Revoked) };
    // Once a minute at most: a view laid out is hundreds of requests.
    if t.used_at.is_none_or(|u| now - u > 60_000) {
        db.conn.execute("UPDATE tokens SET used_at = ?2 WHERE id = ?1", params![t.id, now])?;
    }
    Ok(Checked::Person(u))
}

pub fn token(db: &Db, id: &str) -> Result<Option<TokenInfo>> {
    Ok(db.conn.query_row(&format!("SELECT {TOKEN_COLS} FROM tokens WHERE id = ?1"), params![id], token_row).optional()?)
}

/// The tokens that work, newest first: a person's, or everyone's.
pub fn tokens(db: &Db, person: Option<&str>) -> Result<Vec<TokenInfo>> {
    let mut st = db.conn.prepare(&format!("SELECT {TOKEN_COLS} FROM tokens WHERE (?1 IS NULL OR person = ?1) AND revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2) ORDER BY created_at DESC"))?;
    let rows = st.query_map(params![person, now_ms()], token_row)?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn revoke(db: &Db, id: &str, by: &str) -> Result<TokenInfo> {
    let mut t = token(db, id)?.ok_or_else(|| anyhow!("no token {id}: `avp token list`"))?;
    if t.revoked_at.is_none() {
        let now = now_ms();
        db.conn.execute("UPDATE tokens SET revoked_at = ?2 WHERE id = ?1", params![id, now])?;
        db.emit(SERVER, "token.revoked", None, None, json!({ "id": id, "person": t.person, "name": t.name, "kind": t.kind, "by": by }))?;
        t.revoked_at = Some(now);
    }
    Ok(t)
}

/// When a token ends: `never`, a span from now (`90d`, `12h`, `2w`), or a date.
pub fn expiry(s: &str, now: i64) -> Result<Option<i64>> {
    let s = s.trim();
    if s.is_empty() || s == "never" {
        return Ok(None);
    }
    let span = s.ends_with(['m', 'h', 'd', 'w']) && s[..s.len() - 1].parse::<f64>().is_ok();
    let at = if span { 2 * now - crate::stats::parse_when(s, now)? } else { crate::stats::parse_time(s)? };
    anyhow::ensure!(at > now, "{s}: a token ends after now (90d, 12h, a date to come, or never)");
    Ok(Some(at))
}

// ---- roles ----

/// A person's role on a repo: the one given on the repo, else the one their role in the repo's
/// organization gives (member: write, admin and owner: admin).
pub fn role(db: &Db, repo: &str, person: &str) -> Result<Option<Role>> {
    if let Some(r) = granted(db, repo, person)? {
        return Ok(Some(r));
    }
    let org: Option<String> = db.conn.query_row("SELECT org FROM repos WHERE id = ?1", params![repo], |r| r.get(0)).optional()?.flatten();
    Ok(match org {
        Some(o) => super::orgs::member(db, &o, person)?.map(super::orgs::OrgRole::repo_role),
        None => None,
    })
}

/// The role given to a person on the repo itself.
fn granted(db: &Db, repo: &str, person: &str) -> Result<Option<Role>> {
    let r: Option<String> = db.conn.query_row("SELECT role FROM roles WHERE repo = ?1 AND person = ?2", params![repo, person], |r| r.get(0)).optional()?;
    Ok(r.and_then(|r| Role::parse(&r).ok().flatten()))
}

/// Gives a person a role on a repo (None takes it away), and says which they had.
pub fn set_role(db: &Db, repo: &Repo, person: &str, role: Option<Role>, by: &str) -> Result<Option<Role>> {
    anyhow::ensure!(user(db, person)?.is_some(), "no user {person}: a server admin adds them first (`avp user add {person}`)");
    let was = granted(db, &repo.id, person)?;
    match role {
        Some(r) => db.conn.execute(
            "INSERT INTO roles (repo, person, role) VALUES (?1, ?2, ?3) ON CONFLICT (repo, person) DO UPDATE SET role = excluded.role",
            params![repo.id, person, r.as_str()],
        )?,
        None => db.conn.execute("DELETE FROM roles WHERE repo = ?1 AND person = ?2", params![repo.id, person])?,
    };
    if was != role {
        let name = |r: Option<Role>| r.map(Role::as_str).unwrap_or("none");
        db.emit(&repo.id, "role.set", None, None, json!({ "person": person, "role": name(role), "was": name(was), "repo": repo.name, "by": by }))?;
    }
    Ok(was)
}

/// The people with a role on a repo.
pub fn roles_of_repo(db: &Db, repo: &str) -> Result<Vec<(String, Role)>> {
    let mut st = db.conn.prepare("SELECT person, role FROM roles WHERE repo = ?1 ORDER BY person")?;
    let rows = st.query_map(params![repo], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter_map(|(p, r)| Some((p, Role::parse(&r).ok()??))).collect())
}

/// A person's role on each repo they have one on, by the repo's name (`<space>/<name>` on AVP
/// Cloud): given on the repo, or by their role in its organization.
pub fn roles_of_user(db: &Db, person: &str) -> Result<Vec<(String, Role)>> {
    let mut st = db.conn.prepare(
        "SELECT COALESCE(COALESCE(r.org, r.owner) || '/', '') || r.name, COALESCE(x.role, CASE m.role WHEN 'member' THEN 'write' ELSE 'admin' END) FROM repos r \
         LEFT JOIN roles x ON x.repo = r.id AND x.person = ?1 LEFT JOIN members m ON m.org = r.org AND m.person = ?1 \
         WHERE x.role IS NOT NULL OR m.role IS NOT NULL ORDER BY r.name",
    )?;
    let rows = st.query_map(params![person], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?.into_iter().filter_map(|(p, r)| Some((p, Role::parse(&r).ok()??))).collect())
}

/// The repos a request may see: all of them but for a person who is no admin, who sees those they
/// have a role on.
pub fn readable(db: &Db, repos: Vec<Repo>, who: Option<&Who>) -> Result<Vec<Repo>> {
    let Some(Who::Person(u)) = who.filter(|w| !w.root()) else { return Ok(repos) };
    let mine: std::collections::HashSet<String> = roles_of_user(db, &u.name)?.into_iter().map(|(r, _)| r).collect();
    Ok(repos.into_iter().filter(|r| mine.contains(&r.full_name())).collect())
}

/// A person who creates a repo is its admin; on AVP Cloud it is theirs, in their own space, until
/// they bring it into an organization.
pub fn created(e: &Engine, repo: &Repo, who: Option<&Who>) -> Result<()> {
    if let Some(Who::Person(u)) = who {
        let db = e.db.lock().unwrap();
        set_role(&db, repo, &u.name, Some(Role::Admin), &u.name)?;
        if super::orgs::on() {
            db.conn.execute("UPDATE repos SET owner = ?2 WHERE id = ?1", params![repo.id, u.name])?;
        }
    }
    Ok(())
}

/// The changes of people, tokens, roles and organizations after `since`, oldest first.
pub fn account_events(db: &Db, since: i64, limit: i64) -> Result<Vec<Event>> {
    let mut st = db.conn.prepare(
        "SELECT id, at, kind, task, path, data FROM events WHERE id > ?1 AND ((repo = ?2 AND (kind LIKE 'user.%' OR kind LIKE 'token.%' OR kind LIKE 'org.%')) \
         OR (repo IN (SELECT id FROM repos) AND kind = 'role.set')) ORDER BY id LIMIT ?3",
    )?;
    let rows = st.query_map(params![since, SERVER, limit.clamp(1, 10_000)], |r| {
        Ok(Event { id: r.get(0)?, at: r.get(1)?, kind: r.get(2)?, task: r.get(3)?, path: r.get(4)?, data: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or(serde_json::Value::Null) })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

// ---- the gate: who asks, and whether they may ----

/// With `AVP_TOKEN` set, the server checks every request itself: Basic auth whose password is the
/// token (`https://name:TOKEN@host`, the name is who is asking), or `Bearer TOKEN`. Returns the
/// person, or None when the request does not carry the token.
pub fn token_user(authorization: Option<&str>, token: &str) -> Option<String> {
    use base64::Engine as _;
    let same = |a: &[u8], b: &[u8]| a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0;
    let h = authorization?.trim();
    if let Some(t) = h.strip_prefix("Bearer ") {
        return same(t.trim().as_bytes(), token.as_bytes()).then(|| "token".to_string());
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?.trim()).ok()?;
    let (user, pass) = std::str::from_utf8(&raw).ok()?.split_once(':')?;
    (same(pass.as_bytes(), token.as_bytes()) && !user.is_empty()).then(|| user.to_string())
}


/// What a request needs, read from its method and path alone. A route this does not name needs
/// read to read a repo and write to change it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// Signing in, and AVP Live's page itself (its data asks for more).
    Nothing,
    /// Any person.
    Person,
    /// An admin of the server: people, the workers (a job carries the secrets its steps name), the drain.
    ServerAdmin,
    Repo(String, Role),
    /// A promotion or a rollback: one of the environment's `approvers` in avp.toml, or with none
    /// named, an admin of the repo.
    Promote(String, String),
}

pub fn need(method: &Method, path: &str) -> Need {
    let read = *method == Method::GET || *method == Method::HEAD;
    let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
    match seg.as_slice() {
        // A device login's approval is a signed-in person's; starting and polling one, before any token.
        ["login", "device", "approve"] => Need::Person,
        ["live"] | ["auth", ..] | ["login", ..] | ["install"] => Need::Nothing,
        ["session"] | ["signup"] if *method == Method::POST => Need::Nothing,
        // What the workers run is every organization's: on AVP Cloud, a server admin's to read.
        ["work"] | ["drain"] if read && !super::orgs::on() => Need::Person,
        ["orgs", _, "plan"] => Need::ServerAdmin,
        ["work", ..] | ["drain"] | ["users", ..] | ["accounts", ..] => Need::ServerAdmin,
        // `git clone https://<server>/<repo>.git`: the repo's readers (the route refuses a push).
        [key @ .., "info", "refs"] | [key @ .., "git-upload-pack" | "git-receive-pack"] if git_repo(key).is_some() => Need::Repo(git_repo(key).unwrap_or_default(), Role::Read),
        ["repos", repo, rest @ ..] => {
            let role = match rest {
                ["secrets", ..] => Role::Admin,
                _ if read => Role::Read,
                // POSTs that read: many objects, a diff, the line's memory of some paths.
                ["objects"] | ["diff"] | ["history", "context"] => Role::Read,
                ["envs", env] | ["envs", env, "rollback"] => return Need::Promote(repo.to_string(), env.to_string()),
                ["import"] | ["git", ..] | ["guarded"] | ["landing"] | ["roles"] | ["envs", ..] => Role::Admin,
                _ => Role::Write,
            };
            Need::Repo(repo.to_string(), role)
        }
        _ => Need::Person,
    }
}

/// The repo a git URL names: `<repo>.git`, or `<space>/<repo>.git` with its organization (or the
/// person whose own it is) in front, which only reaches that space's repo.
fn git_repo(seg: &[&str]) -> Option<String> {
    let name = |r: &str| r.strip_suffix(".git").filter(|n| !n.is_empty()).map(String::from);
    match seg {
        [repo] => name(repo),
        [space, repo] => name(repo).map(|r| format!("{space}/{r}")),
        _ => None,
    }
}

#[derive(Clone)]
pub struct Gate {
    engine: Engine,
    /// The server's own key, `AVP_TOKEN`.
    key: Option<std::sync::Arc<String>>,
    known: Known,
}

impl Gate {
    pub fn new(engine: Engine) -> Self {
        Self { engine, key: std::env::var("AVP_TOKEN").ok().filter(|t| !t.is_empty()).map(std::sync::Arc::new), known: Known::default() }
    }
}

/// Whether the server has accounts, as the gate last read it: read once, not at every request (a
/// server without accounts answers as before, the database untouched), and again once a person is
/// added or removed (the routes that do it get this as a request extension).
#[derive(Clone, Default)]
pub struct Known(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl Known {
    const UNREAD: u8 = 0;
    const OFF: u8 = 1;
    const ON: u8 = 2;

    fn on(&self, db: &std::sync::Mutex<Db>) -> Result<bool> {
        use std::sync::atomic::Ordering;
        match self.0.load(Ordering::Acquire) {
            Self::OFF => Ok(false),
            Self::ON => Ok(true),
            _ => {
                let on = on(&db.lock().unwrap())?;
                self.0.store(if on { Self::ON } else { Self::OFF }, Ordering::Release);
                Ok(on)
            }
        }
    }

    fn forget(&self) {
        self.0.store(Self::UNREAD, std::sync::atomic::Ordering::Release);
    }
}

/// What the gate reads of a request.
struct Ask {
    authorization: Option<String>,
    /// AVP Live's session, taken for a GET, or for a request that says it comes from AVP Live
    /// (`x-avp-live`), which a form on another site cannot send.
    session: Option<String>,
    /// From the server's own machine: a loopback address no proxy forwarded.
    machine: bool,
}

fn json_refusal(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn unsigned(msg: impl Into<String>) -> Response {
    json_refusal(StatusCode::UNAUTHORIZED, msg)
}

/// Every request goes through here: who sends it (their token, the server's key, the server's own
/// machine), whether they may, and the person the routes then read (`x-avp-user`), never one the
/// client chose.
pub async fn gate(State(g): State<Gate>, mut req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let (method, path) = (req.method().clone(), req.uri().path().to_string());
    let git = path.contains(".git/");
    let h = req.headers();
    let live_ok = matches!(*req.method(), Method::GET | Method::HEAD) || h.contains_key("x-avp-live");
    let ask = Ask {
        authorization: h.get("authorization").and_then(|v| v.to_str().ok()).map(String::from),
        session: cookie(h, SESSION).filter(|_| live_ok),
        machine: machine(&req),
    };
    req.extensions_mut().insert(g.known.clone());
    let engine = g.engine.clone();
    let decided = tokio::task::spawn_blocking(move || g.decide(&ask, &method, &path)).await;
    let who = match decided {
        Ok(Ok((who, named))) => {
            // The routes read the repo by its id: the gate ran before them, so they match this path.
            if let Some(path) = named {
                let query = req.uri().query().map(|q| format!("?{q}")).unwrap_or_default();
                if let Ok(uri) = format!("{path}{query}").parse() {
                    *req.uri_mut() = uri;
                }
            }
            match who {
                Some(who) => who,
                None => return next.run(req).await,
            }
        }
        Ok(Err(mut refusal)) => {
            // git sends its credentials only once asked for them.
            if git && refusal.status() == StatusCode::UNAUTHORIZED {
                refusal.headers_mut().insert("www-authenticate", axum::http::HeaderValue::from_static("Basic realm=\"avp\""));
            }
            return refusal;
        }
        Err(e) => return json_refusal(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    let person = match &who {
        Who::Person(u) => Some(u.name.as_str()),
        Who::Server(p) => p.as_deref(),
        Who::Anyone => None,
    };
    if let Some(v) = person.and_then(|p| axum::http::HeaderValue::from_str(p).ok()) {
        req.headers_mut().insert("x-avp-user", v);
    }
    req.extensions_mut().insert(who.clone());
    if super::orgs::on() {
        return tenant(engine, &who, req, next).await;
    }
    next.run(req).await
}

/// On AVP Cloud, what a role on the repo does not settle. One store holds every organization's
/// objects, and a hash known is no right to read one: a person reads the objects their repo holds.
/// A proof runs while the repo's space has minutes left on its plan.
async fn tenant(e: Engine, who: &Who, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let path = req.uri().path().trim_matches('/').to_string();
    let seg: Vec<&str> = path.split('/').collect();
    let reader = matches!(who, Who::Person(u) if !u.admin);
    let method = req.method().clone();
    let read = method == Method::GET || method == Method::HEAD;
    let missing = |hash: &str| json_refusal(StatusCode::NOT_FOUND, format!("no object {hash}"));
    match seg.as_slice() {
        // A repo goes in its space: the asker's own, or an organization's (`acme/shop`).
        ["repos"] if method == Method::POST => {
            let Ok(bytes) = axum::body::to_bytes(req.into_body(), 1 << 20).await else { return json_refusal(StatusCode::BAD_REQUEST, "a repo is asked for by its name and policy") };
            #[derive(Deserialize)]
            struct New {
                name: String,
                #[serde(default)]
                policy: Option<LeasePolicy>,
            }
            let asked: New = match serde_json::from_slice(&bytes) {
                Ok(n) => n,
                Err(x) => return json_refusal(StatusCode::BAD_REQUEST, format!("a repo is asked for by its name and policy: {x}")),
            };
            let who = who.clone();
            match tokio::task::spawn_blocking(move || super::orgs::create_repo(&e, &asked.name, asked.policy.unwrap_or(LeasePolicy::Advisory), &who)).await {
                Ok(Ok(r)) => Json(r).into_response(),
                Ok(Err(x)) => crate::server::err(x),
                Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
            }
        }
        ["repos", repo, "objects", hash] if read && reader => {
            let (e2, repo, hash) = (e.clone(), repo.to_string(), hash.to_string());
            match tokio::task::spawn_blocking(move || holds(&e2, &repo, &[hash])).await {
                Ok(Ok(None)) => next.run(req).await,
                Ok(Ok(Some(h))) => missing(&h),
                Ok(Err(x)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{x:#}")),
                Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
            }
        }
        // A batch names its objects in its body: read here, then handed on as it came.
        ["repos", repo, "objects"] if method == Method::POST && reader => {
            let repo = repo.to_string();
            let (parts, body) = req.into_parts();
            let Ok(bytes) = axum::body::to_bytes(body, 64 << 20).await else { return json_refusal(StatusCode::BAD_REQUEST, "the list of objects is too long") };
            let hashes: Vec<String> = serde_json::from_slice::<serde_json::Value>(&bytes).ok().and_then(|v| serde_json::from_value(v["hashes"].clone()).ok()).unwrap_or_default();
            let e2 = e.clone();
            match tokio::task::spawn_blocking(move || holds(&e2, &repo, &hashes)).await {
                Ok(Ok(None)) => next.run(axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes))).await,
                Ok(Ok(Some(h))) => missing(&h),
                Ok(Err(x)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{x:#}")),
                Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
            }
        }
        // Sent to the repo: from now on it holds it.
        ["repos", repo, "objects", hash] if method == Method::PUT => {
            let (repo, hash) = (repo.to_string(), hash.to_string());
            let res = next.run(req).await;
            if res.status().is_success() {
                let kept = tokio::task::spawn_blocking(move || -> Result<()> {
                    let db = e.db.lock().unwrap();
                    let id = db.repo(&repo)?.id;
                    db.conn.execute("INSERT OR IGNORE INTO repo_objects (repo, hash) VALUES (?1, ?2)", params![id, hash])?;
                    Ok(())
                })
                .await;
                if let Ok(Err(x)) = kept {
                    eprintln!("object of {path} not recorded: {x:#}");
                }
            }
            res
        }
        ["repos", repo, "tasks", _, "land" | "check" | "speculate"] | ["repos", repo, "full"] if method == Method::POST => {
            let repo = repo.to_string();
            match tokio::task::spawn_blocking(move || super::orgs::may_prove(&e, &repo)).await {
                Ok(Ok(Ok(()))) => next.run(req).await,
                Ok(Ok(Err(over))) => json_refusal(StatusCode::PAYMENT_REQUIRED, over),
                // A repo the gate let through but cannot find: the route says so.
                Ok(Err(_)) => next.run(req).await,
                Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
            }
        }
        _ => next.run(req).await,
    }
}

/// The first of these objects the repo does not hold (None: it holds them all). It holds what was
/// sent to it, what its tasks wrote, its line's trees and what is in them: the trees not read yet
/// are read, newest first, until the object is found, and what they hold is remembered.
pub fn holds(e: &Engine, repo: &str, hashes: &[String]) -> Result<Option<String>> {
    let db = e.db.lock().unwrap();
    let repo = db.repo(repo)?.id;
    let known = |hash: &str| -> Result<bool> {
        let one = |sql: &str| db.conn.query_row(sql, params![repo, hash], |r| r.get::<_, bool>(0));
        Ok(one("SELECT EXISTS (SELECT 1 FROM repo_objects WHERE repo = ?1 AND hash = ?2)")?
            || one("SELECT EXISTS (SELECT 1 FROM line WHERE repo = ?1 AND tree = ?2)")?
            || one("SELECT EXISTS (SELECT 1 FROM writes WHERE blob = ?2 AND task IN (SELECT id FROM tasks WHERE repo = ?1))")?)
    };
    let mut unread: Option<Vec<String>> = None;
    for hash in hashes {
        if known(hash)? {
            continue;
        }
        let trees = match &mut unread {
            Some(t) => t,
            None => {
                let mut st = db.conn.prepare("SELECT tree FROM line WHERE repo = ?1 AND tree NOT IN (SELECT tree FROM repo_trees WHERE repo = ?1) GROUP BY tree ORDER BY MAX(position) DESC")?;
                let rows = st.query_map(params![repo], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?;
                unread.insert(rows)
            }
        };
        let mut found = false;
        while !found && !trees.is_empty() {
            let t = trees.remove(0);
            let Ok(tree) = e.objects.get_tree(&t) else { continue };
            let tx = db.conn.unchecked_transaction()?;
            for entry in tree.values() {
                tx.execute("INSERT OR IGNORE INTO repo_objects (repo, hash) VALUES (?1, ?2)", params![repo, entry.blob])?;
            }
            tx.execute("INSERT OR IGNORE INTO repo_trees (repo, tree) VALUES (?1, ?2)", params![repo, t])?;
            tx.commit()?;
            found = tree.values().any(|x| &x.blob == hash);
        }
        if !found {
            return Ok(Some(hash.clone()));
        }
    }
    Ok(None)
}

impl Gate {
    /// Who asks, whether they may, and on AVP Cloud the path with the repo it names said by its id
    /// (see `named`).
    fn decide(&self, ask: &Ask, method: &Method, path: &str) -> Result<(Option<Who>, Option<String>), Response> {
        let on = self.known.on(&self.engine.db).map_err(|e| json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        if !on {
            // As before accounts: the server's token when it has one, nothing otherwise.
            return match &self.key {
                None => Ok((Some(Who::Anyone), None)),
                Some(key) => match token_user(ask.authorization.as_deref(), key) {
                    Some(person) => Ok((Some(Who::Server(Some(person))), None)),
                    None => Err((StatusCode::UNAUTHORIZED, [("www-authenticate", "Basic realm=\"avp\"")], "AVP_TOKEN required").into_response()),
                },
            };
        }
        let who = match self.identify(ask) {
            Ok(who) => who,
            Err(_) if need(method, path) == Need::Nothing => return Ok((None, None)),
            Err(refusal) => return Err(refusal),
        };
        let named = if super::orgs::on() { self.named(path, &who)? } else { None };
        self.allow(&who, &need(method, named.as_deref().unwrap_or(path)))?;
        Ok((Some(who), named))
    }

    /// On AVP Cloud, a path to a repo with the repo said by its id: `/repos/acme/shop/line` (or
    /// `/repos/acme%2Fshop/line`) is acme's `shop`; `/repos/shop/line`, the asker's one `shop` when
    /// other spaces have one too. None: the path stays as it is.
    fn named(&self, path: &str, who: &Who) -> Result<Option<String>, Response> {
        let Some(rest) = path.strip_prefix("/repos/") else { return Ok(None) };
        let seg: Vec<&str> = rest.split('/').collect();
        let failed = |e: anyhow::Error| json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
        let to = |id: &str, after: &[&str]| Some(format!("/repos/{id}{}", after.iter().map(|s| format!("/{s}")).collect::<String>()));
        let db = self.engine.db.lock().unwrap();
        let first = seg[0].replace("%2F", "/").replace("%2f", "/");
        if let Some((space, name)) = first.split_once('/') {
            return Ok(db.repo_in(Some(space), name).map_err(failed)?.and_then(|id| to(&id, &seg[1..])));
        }
        if let [space, name, after @ ..] = seg.as_slice() {
            if let Some(id) = db.repo_in(Some(space), name).map_err(failed)? {
                return Ok(to(&id, after));
            }
        }
        let same = db.repos_named(&first).map_err(failed)?;
        if same.len() < 2 {
            return Ok(None);
        }
        let mut mine = vec![];
        for r in same {
            let reads = match who {
                Who::Person(u) if !u.admin => role(&db, &r.id, &u.name).map_err(failed)?.is_some(),
                _ => true,
            };
            if reads {
                mine.push(r);
            }
        }
        match mine.as_slice() {
            // None of theirs: the route answers there is no such repo, as for any other.
            [] => Ok(None),
            [one] => Ok(to(&one.id, &seg[1..])),
            many => Err(json_refusal(
                StatusCode::CONFLICT,
                format!("{first} names {} of your repos: {}. Say which, as `<space>/{first}`", many.len(), many.iter().map(Repo::full_name).collect::<Vec<_>>().join(", ")),
            )),
        }
    }

    fn identify(&self, ask: &Ask) -> Result<Who, Response> {
        let presented = presented(ask.authorization.as_deref()).or(ask.session.clone());
        if let Some(t) = &presented {
            let checked = check_token(&self.engine.db.lock().unwrap(), t).map_err(|e| json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
            match checked {
                Checked::Person(u) => return Ok(Who::Person(u)),
                Checked::Revoked => return Err(unsigned("this token was revoked: `avp token create` makes another (signed in another way), or a server admin's `avp token create --user <you>`")),
                Checked::Expired(at) => return Err(unsigned(format!("this token expired on {} UTC: `avp token create` makes another (signed in another way), or a server admin's `avp token create --user <you>`", crate::blame::utc(at)))),
                Checked::Unknown => {}
            }
        }
        if let Some(person) = self.key.as_deref().and_then(|key| token_user(ask.authorization.as_deref(), key)) {
            return Ok(Who::Server(Some(person)));
        }
        if presented.is_none() && self.key.is_none() && ask.machine {
            return Ok(Who::Server(None));
        }
        Err(unsigned(match presented {
            Some(_) => "this token is none of this server's: a person's own goes in AVP_TOKEN (`avp token list` on a machine that works shows yours)",
            None => "this server has accounts: put a token of yours in AVP_TOKEN (a server admin's `avp user add <you>` gives the first), or sign in to AVP Live",
        }))
    }

    fn allow(&self, who: &Who, need: &Need) -> Result<(), Response> {
        let Who::Person(u) = who else { return Ok(()) };
        if u.admin {
            return Ok(());
        }
        let forbidden = |m: String| json_refusal(StatusCode::FORBIDDEN, m);
        match need {
            Need::Nothing | Need::Person => Ok(()),
            Need::ServerAdmin => Err(forbidden(format!("{} is no admin of this server: people, tokens of others and workers are theirs (`avp user admin {}`, by one)", u.name, u.name))),
            Need::Repo(key, want) => self.repo_role(u, key, *want).map(|_| ()),
            Need::Promote(key, env) => {
                let (repo, has) = self.repo_role(u, key, Role::Read)?;
                if has == Role::Admin {
                    return Ok(());
                }
                // Named in the list: the engine checks the person against it, as for anyone.
                let listed = self.engine.env_config(&repo.id).ok().and_then(|c| c.get(env.as_str()).map(|c| !c.approvers.is_empty())).unwrap_or(false);
                if listed {
                    return Ok(());
                }
                Err(forbidden(format!("{env} names no promoters in avp.toml: an admin of {} promotes it and rolls it back, or `[envs.{env}] promoters = [\"{}\"]` lets them", repo.name, u.name)))
            }
        }
    }

    /// The repo and the person's role on it, at least `want`. A repo they have no role on answers
    /// as one that does not exist: no hint that the name is taken.
    fn repo_role(&self, u: &User, key: &str, want: Role) -> Result<(Repo, Role), Response> {
        let db = self.engine.db.lock().unwrap();
        let none = || json_refusal(StatusCode::NOT_FOUND, format!("no repo {key} (or none you have a role on: an admin of it gives one, `avp user role {} read --repo {key}`)", u.name));
        let Ok(repo) = db.repo(key) else { return Err(none()) };
        match role(&db, &repo.id, &u.name).map_err(|e| json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))? {
            None => Err(none()),
            Some(has) if has < want => Err(json_refusal(
                StatusCode::FORBIDDEN,
                format!("{} has {} on {} ({}): this needs {} ({}). An admin of {} gives it: `avp user role {} {} --repo {}`", u.name, has.as_str(), repo.name, has.does(), want.as_str(), want.does(), repo.name, u.name, want.as_str(), repo.name),
            )),
            Some(has) => Ok((repo, has)),
        }
    }
}

/// The token a request carries: `Bearer <token>`, or the password of Basic auth (whatever the name).
fn presented(authorization: Option<&str>) -> Option<String> {
    use base64::Engine as _;
    let h = authorization?.trim();
    if let Some(t) = h.strip_prefix("Bearer ") {
        return Some(t.trim().to_string()).filter(|t| !t.is_empty());
    }
    let raw = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?.trim()).ok()?;
    let (_, pass) = std::str::from_utf8(&raw).ok()?.split_once(':')?;
    Some(pass.to_string()).filter(|p| !p.is_empty())
}

/// A request from the server's own machine: a loopback address that no proxy forwarded (the one in
/// front of a shared server runs on the same machine, and says whom it forwards).
fn machine(req: &axum::extract::Request) -> bool {
    let loopback = req.extensions().get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().is_some_and(|c| c.0.ip().to_canonical().is_loopback());
    loopback && !["forwarded", "x-forwarded-for", "x-real-ip", "x-avp-user"].iter().any(|h| req.headers().contains_key(*h))
}

fn cookie(h: &HeaderMap, name: &str) -> Option<String> {
    h.get_all("cookie").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).find_map(|c| c.trim().strip_prefix(name)?.strip_prefix('=').map(String::from)).filter(|v| !v.is_empty())
}

/// A cookie of AVP Live's: out of the page's scripts' reach, sent only over HTTPS when the proxy in
/// front says the request came that way.
fn set_cookie(h: &HeaderMap, name: &str, value: &str, max_age_s: i64, same_site: &str) -> String {
    let https = h.get("x-forwarded-proto").and_then(|v| v.to_str().ok()).is_some_and(|p| p.eq_ignore_ascii_case("https"));
    format!("{name}={value}; Path=/; Max-Age={max_age_s}; HttpOnly; SameSite={same_site}{}", if https { "; Secure" } else { "" })
}

// ---- the routes ----

type S = State<Engine>;

pub(crate) async fn run<T: Serialize + Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Response {
    let out = tokio::task::spawn_blocking(f).await.map_err(|e| anyhow!(e)).and_then(|r| r);
    match out {
        Ok(v) => Json(v).into_response(),
        Err(e) if e.is::<Forbidden>() => json_refusal(StatusCode::FORBIDDEN, e.to_string()),
        Err(e) => {
            let (code, body) = crate::server::err_parts(&e);
            (code, Json(body)).into_response()
        }
    }
}

pub fn routes() -> Router<Engine> {
    Router::new()
        .route("/me", get(me))
        .route("/users", get(list_users).post(add_user_route))
        .route("/users/{name}/remove", post(remove_user_route))
        .route("/users/{name}/admin", post(admin_route))
        .route("/repos/{repo}/roles", get(repo_roles).post(set_role_route))
        .route("/tokens", get(list_tokens).post(create_token_route))
        .route("/tokens/{id}/revoke", post(revoke_route))
        .route("/accounts/events", get(events_route))
        .route("/session", post(sign_in).delete(sign_out))
        .route("/signup", post(sign_up))
        .route("/auth", get(ways))
        .route("/auth/github", get(github_start))
        .route("/auth/github/callback", get(github_back))
        .route("/login/device", get(device_page).post(device_create))
        .route("/login/device/peek", get(device_peek))
        .route("/login/device/approve", post(device_approve))
        .route("/login/device/token", post(device_token))
        .route("/install", get(install_script))
}

/// Who the server takes the request for, and their roles: `avp whoami`, AVP Live's corner.
async fn me(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let roles = match &who {
            Who::Person(u) => roles_of_user(&db, &u.name)?,
            _ => vec![],
        };
        let person = match &who {
            Who::Person(u) => Some(u.name.clone()),
            Who::Server(p) => p.clone(),
            Who::Anyone => headers.get("x-avp-user").and_then(|v| v.to_str().ok()).map(String::from),
        };
        let orgs = match &who {
            Who::Person(u) if super::orgs::on() => super::orgs::orgs_of(&db, &u.name)?.into_iter().map(|(o, r)| json!({ "org": o.name, "role": r, "plan": o.plan })).collect(),
            _ => vec![],
        };
        Ok(json!({
            "accounts": on(&db)?, "person": person, "admin": who.root(), "via": who.via(), "github": github_app().is_some(),
            "roles": roles.iter().map(|(r, role)| json!({ "repo": r, "role": role })).collect::<Vec<_>>(),
            "cloud": super::orgs::on(), "orgs": orgs,
        }))
    })
    .await
}

#[derive(Deserialize)]
struct AddUser {
    name: String,
    email: Option<String>,
    github: Option<String>,
    #[serde(default)]
    admin: bool,
}

async fn add_user_route(State(e): S, Extension(who): Extension<Who>, Extension(known): Extension<Known>, headers: HeaderMap, Json(b): Json<AddUser>) -> Response {
    run(move || {
        let added = add_user(&e.db.lock().unwrap(), b.name.trim(), b.email.as_deref(), b.github.as_deref(), b.admin, &who.by(&headers));
        known.forget();
        let (user, token) = added?;
        Ok(json!({ "user": user, "token": token }))
    })
    .await
}

async fn list_users(State(e): S) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let mut out = vec![];
        for u in users(&db)? {
            let roles: Vec<_> = roles_of_user(&db, &u.name)?.into_iter().map(|(r, role)| json!({ "repo": r, "role": role })).collect();
            let tokens = tokens(&db, Some(&u.name))?.len();
            out.push(json!({ "user": u, "roles": roles, "tokens": tokens }));
        }
        Ok(out)
    })
    .await
}

async fn remove_user_route(State(e): S, Extension(who): Extension<Who>, Extension(known): Extension<Known>, headers: HeaderMap, P(name): P<String>) -> Response {
    run(move || {
        let removed = remove_user(&e.db.lock().unwrap(), &name, &who.by(&headers));
        known.forget();
        removed?;
        Ok(json!({ "removed": name }))
    })
    .await
}

#[derive(Deserialize)]
struct Admin {
    admin: bool,
}

async fn admin_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(name): P<String>, Json(b): Json<Admin>) -> Response {
    run(move || set_admin(&e.db.lock().unwrap(), &name, b.admin, &who.by(&headers))).await
}

async fn repo_roles(State(e): S, P(repo): P<String>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = db.repo(&repo)?;
        Ok(roles_of_repo(&db, &r.id)?.into_iter().map(|(p, role)| json!({ "person": p, "role": role })).collect::<Vec<_>>())
    })
    .await
}

#[derive(Deserialize)]
struct SetRole {
    person: String,
    role: String,
}

async fn set_role_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<SetRole>) -> Response {
    run(move || {
        let role = Role::parse(&b.role)?;
        let db = e.db.lock().unwrap();
        let r = db.repo(&repo)?;
        let was = set_role(&db, &r, &b.person, role, &who.by(&headers))?;
        Ok(json!({ "person": b.person, "repo": r.name, "role": role.map(Role::as_str).unwrap_or("none"), "was": was.map(Role::as_str).unwrap_or("none") }))
    })
    .await
}

#[derive(Deserialize)]
struct CreateToken {
    name: Option<String>,
    expires: Option<String>,
    /// Another person's: a server admin makes it.
    person: Option<String>,
}

async fn create_token_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, Json(b): Json<CreateToken>) -> Response {
    run(move || {
        let person = match (b.person, who.person()) {
            (Some(p), me) if me != Some(p.as_str()) => {
                anyhow::ensure!(who.root(), Forbidden(format!("a token for {p} is a server admin's to make: `avp token create` makes one of yours")));
                p
            }
            (Some(p), _) => p,
            (None, Some(me)) => me.to_string(),
            (None, None) => anyhow::bail!("the server's key is no person's: `avp token create --user <name>`"),
        };
        let expires = expiry(b.expires.as_deref().unwrap_or("never"), now_ms())?;
        let db = e.db.lock().unwrap();
        anyhow::ensure!(user(&db, &person)?.is_some(), "no user {person}: `avp user add {person}` first");
        create_token(&db, &person, b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or("token"), "token", expires, &who.by(&headers))
    })
    .await
}

#[derive(Deserialize)]
struct TokensQuery {
    person: Option<String>,
}

async fn list_tokens(State(e): S, Extension(who): Extension<Who>, Query(q): Query<TokensQuery>) -> Response {
    run(move || {
        let person = q.person.or_else(|| who.person().map(String::from));
        anyhow::ensure!(who.root() || person.as_deref() == who.person(), Forbidden("another person's tokens are a server admin's to read".into()));
        tokens(&e.db.lock().unwrap(), person.as_deref())
    })
    .await
}

async fn revoke_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(id): P<String>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let t = token(&db, &id)?;
        // Another person's token answers as one that does not exist.
        anyhow::ensure!(t.is_some_and(|t| who.root() || who.person() == Some(t.person.as_str())), "no token {id}: `avp token list`");
        revoke(&db, &id, &who.by(&headers))
    })
    .await
}

#[derive(Deserialize)]
struct EventsQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn events_route(State(e): S, Query(q): Query<EventsQuery>) -> Response {
    run(move || account_events(&e.db.lock().unwrap(), q.since.unwrap_or(0), q.limit.unwrap_or(1000))).await
}

// ---- AVP Live's sign-in ----

#[derive(Deserialize)]
struct SignIn {
    #[serde(default)]
    token: String,
    /// On AVP Cloud: a name or an email, and its password.
    name: Option<String>,
    password: Option<String>,
}

/// A person's token in, a session of AVP Live's out, as a cookie: the token itself never sits in
/// the browser. On AVP Cloud, a name (or email) and its password too: AVP Live gets its session,
/// any other client a token of 90 days (`avp user login`).
async fn sign_in(State(e): S, ext: Extensions, headers: HeaderMap, Json(b): Json<SignIn>) -> Response {
    if let (Some(name), Some(password)) = (b.name.clone(), b.password.clone()) {
        return password_sign_in(e, address(&headers, &ext), headers, name, password).await;
    }
    if !headers.contains_key("x-avp-live") {
        return json_refusal(StatusCode::BAD_REQUEST, "signing in is AVP Live's: open /live");
    }
    let h = headers.clone();
    let out = tokio::task::spawn_blocking(move || -> Result<Result<NewToken, Response>> {
        let db = e.db.lock().unwrap();
        if !on(&db)? {
            return Ok(Err(json_refusal(StatusCode::BAD_REQUEST, "this server has no accounts: AVP Live needs no sign-in")));
        }
        match check_token(&db, b.token.trim())? {
            Checked::Person(u) => Ok(Ok(session(&db, &u.name, "live")?)),
            _ => Ok(Err(unsigned("this token is none of this server's, or no longer works: `avp token create` makes one"))),
        }
    })
    .await;
    match out {
        Ok(Ok(Ok(s))) => signed_in(&h, &s, Json(json!({ "person": s.info.person })).into_response()),
        Ok(Ok(Err(refusal))) => refusal,
        Ok(Err(e)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn session(db: &Db, person: &str, via: &str) -> Result<NewToken> {
    create_token(db, person, &format!("AVP Live ({via})"), "session", Some(now_ms() + SESSION_DAYS * 86_400_000), person)
}

fn signed_in(h: &HeaderMap, s: &NewToken, mut res: Response) -> Response {
    if let Ok(v) = axum::http::HeaderValue::from_str(&set_cookie(h, SESSION, &s.token, SESSION_DAYS * 86_400, "Strict")) {
        res.headers_mut().append("set-cookie", v);
    }
    res
}

/// Ends AVP Live's session: its token is revoked, the cookie forgotten.
async fn sign_out(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap) -> Response {
    let session = cookie(&headers, SESSION);
    let by = who.by(&headers);
    let done = tokio::task::spawn_blocking(move || -> Result<()> {
        let Some(s) = session else { return Ok(()) };
        let db = e.db.lock().unwrap();
        let id: Option<String> = db.conn.query_row("SELECT id FROM tokens WHERE hash = ?1 AND kind = 'session'", params![hash(&s)], |r| r.get(0)).optional()?;
        if let Some(id) = id {
            revoke(&db, &id, &by)?;
        }
        Ok(())
    })
    .await;
    let mut res = match done {
        Ok(Ok(())) => Json(json!({ "ok": true })).into_response(),
        Ok(Err(e)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    };
    if let Ok(v) = axum::http::HeaderValue::from_str(&set_cookie(&headers, SESSION, "", 0, "Strict")) {
        res.headers_mut().append("set-cookie", v);
    }
    res
}

// ---- AVP Cloud's sign-up and passwords ----

/// Who asks, for the limits on sign-ups and sign-ins: the address the proxy in front appended
/// (the last of `x-forwarded-for`, the ones before it are the client's to make up), else the peer's.
fn address(h: &HeaderMap, ext: &Extensions) -> String {
    let forwarded = h.get_all("x-forwarded-for").iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).map(str::trim).filter(|v| !v.is_empty()).last().map(String::from);
    forwarded
        .or_else(|| h.get("x-real-ip").and_then(|v| v.to_str().ok()).map(|v| v.trim().to_string()))
        .or_else(|| ext.get::<axum::extract::ConnectInfo<std::net::SocketAddr>>().map(|c| c.0.ip().to_canonical().to_string()))
        .unwrap_or_default()
}

/// Sign-ups an address may make an hour, and sign-ins it may try in 10 minutes: an open server is
/// not a script's to fill with accounts or to guess passwords on.
const SIGNUPS_AN_HOUR: usize = 5;
const SIGNINS_IN_10_MIN: usize = 10;

/// Whether `address` may try `what` once more: at most `most` in `window_ms`.
fn within_limit(what: &str, address: &str, most: usize, window_ms: i64) -> bool {
    type Tries = std::collections::HashMap<(String, String), Vec<i64>>;
    static TRIES: std::sync::Mutex<Option<Tries>> = std::sync::Mutex::new(None);
    let now = now_ms();
    let mut all = TRIES.lock().unwrap();
    let all = all.get_or_insert_with(Default::default);
    // The addresses gone quiet are forgotten, so the map does not grow forever.
    if all.len() > 10_000 {
        all.retain(|_, at| at.iter().any(|t| now - t < 3_600_000));
    }
    let at = all.entry((what.to_string(), address.to_string())).or_default();
    at.retain(|t| now - t < window_ms);
    if at.len() >= most {
        return false;
    }
    at.push(now);
    true
}

fn too_many(what: &str) -> Response {
    json_refusal(StatusCode::TOO_MANY_REQUESTS, format!("too many {what} from this address: try again later"))
}

/// A password as the server keeps it: argon2id, salted, never the password.
fn password_hash(password: &str) -> Result<String> {
    use argon2::password_hash::{PasswordHasher, SaltString};
    use base64::Engine as _;
    anyhow::ensure!((10..=256).contains(&password.chars().count()), "a password is 10 characters at least, 256 at most");
    let random = fresh()?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&random[4..])?;
    let salt = SaltString::encode_b64(&bytes[..16]).map_err(|e| anyhow!("salt: {e}"))?;
    Ok(argon2::Argon2::default().hash_password(password.as_bytes(), &salt).map_err(|e| anyhow!("password: {e}"))?.to_string())
}

pub fn set_password(db: &Db, person: &str, password: &str) -> Result<()> {
    let hash = password_hash(password)?;
    db.conn.execute(
        "INSERT INTO passwords (person, hash, set_at) VALUES (?1, ?2, ?3) ON CONFLICT (person) DO UPDATE SET hash = excluded.hash, set_at = excluded.set_at",
        params![person, hash, now_ms()],
    )?;
    db.emit(SERVER, "user.password", None, None, json!({ "name": person, "by": person }))?;
    Ok(())
}

/// The person a name or an email and a password sign in, if they do.
pub fn check_password(db: &Db, name: &str, password: &str) -> Result<Option<User>> {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    let name = name.trim();
    let person: Option<String> = db.conn.query_row("SELECT name FROM users WHERE name = ?1 OR (?1 LIKE '%@%' AND lower(email) = lower(?1))", params![name], |r| r.get(0)).optional()?;
    let Some(person) = person else { return Ok(None) };
    let hash: Option<String> = db.conn.query_row("SELECT hash FROM passwords WHERE person = ?1", params![person], |r| r.get(0)).optional()?;
    let ok = hash.is_some_and(|h| PasswordHash::new(&h).is_ok_and(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok()));
    if ok { user(db, &person) } else { Ok(None) }
}

/// One who signs up on AVP Cloud: a person who is no admin, whose repos are their own until they
/// bring them into an organization.
pub fn sign_up_person(db: &Db, name: &str, email: &str, password: &str) -> Result<(User, NewToken)> {
    anyhow::ensure!(super::orgs::signup_open(), Forbidden("signing up is closed on this server: a server admin adds people (`avp user add`)".into()));
    let email = email.trim();
    anyhow::ensure!(email.contains('@') && !email.contains(char::is_whitespace), "{email:?} is no email address");
    let taken: bool = db.conn.query_row("SELECT EXISTS (SELECT 1 FROM users WHERE lower(email) = lower(?1))", params![email], |r| r.get(0))?;
    anyhow::ensure!(!taken, "{email} already signs a person in: sign in with it (`avp user login {email}`)");
    // The password is checked before anyone is added.
    let hash = password_hash(password)?;
    let (u, token) = add_person(db, name, Some(email), None, false, "sign-up")?;
    db.conn.execute("INSERT INTO passwords (person, hash, set_at) VALUES (?1, ?2, ?3)", params![u.name, hash, now_ms()])?;
    Ok((u, token))
}

#[derive(Deserialize)]
struct SignUp {
    name: String,
    email: String,
    password: String,
}

/// `POST /signup`: an account, its first token, and from AVP Live its session too.
async fn sign_up(State(e): S, Extension(known): Extension<Known>, ext: Extensions, headers: HeaderMap, Json(b): Json<SignUp>) -> Response {
    if !super::orgs::signup_open() {
        return json_refusal(StatusCode::FORBIDDEN, "signing up is closed on this server: a server admin adds people (`avp user add`)");
    }
    if !within_limit("signup", &address(&headers, &ext), SIGNUPS_AN_HOUR, 3_600_000) {
        return too_many("sign-ups");
    }
    let live = headers.contains_key("x-avp-live");
    let out = tokio::task::spawn_blocking(move || -> Result<(User, NewToken, Option<NewToken>)> {
        let db = e.db.lock().unwrap();
        let made = sign_up_person(&db, b.name.trim(), &b.email, &b.password);
        known.forget();
        let (u, token) = made?;
        let s = if live { Some(session(&db, &u.name, "sign-up")?) } else { None };
        Ok((u, token, s))
    })
    .await;
    match out {
        Ok(Ok((u, token, s))) => {
            let res = Json(json!({ "user": u, "token": token })).into_response();
            match s {
                Some(s) => signed_in(&headers, &s, res),
                None => res,
            }
        }
        Ok(Err(x)) if x.is::<Forbidden>() => json_refusal(StatusCode::FORBIDDEN, x.to_string()),
        Ok(Err(x)) => json_refusal(StatusCode::BAD_REQUEST, format!("{x:#}")),
        Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
    }
}

async fn password_sign_in(e: Engine, address: String, headers: HeaderMap, name: String, password: String) -> Response {
    if !super::orgs::on() {
        return json_refusal(StatusCode::BAD_REQUEST, "this server signs in with tokens: put yours in AVP_TOKEN (passwords are AVP Cloud's)");
    }
    if !within_limit("signin", &address, SIGNINS_IN_10_MIN, 600_000) {
        return too_many("sign-ins");
    }
    let live = headers.contains_key("x-avp-live");
    let out = tokio::task::spawn_blocking(move || -> Result<Option<NewToken>> {
        let db = e.db.lock().unwrap();
        let Some(u) = check_password(&db, &name, &password)? else { return Ok(None) };
        Ok(Some(if live { session(&db, &u.name, "password")? } else { create_token(&db, &u.name, "signed in", "token", Some(now_ms() + 90 * 86_400_000), &u.name)? }))
    })
    .await;
    match out {
        Ok(Ok(Some(t))) if live => signed_in(&headers, &t, Json(json!({ "person": t.info.person })).into_response()),
        Ok(Ok(Some(t))) => Json(json!({ "person": t.info.person, "token": t })).into_response(),
        Ok(Ok(None)) => unsigned("no such name (or email) and password here"),
        Ok(Err(x)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{x:#}")),
        Err(x) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, x.to_string()),
    }
}

/// How one signs in here: whether accounts are on, and GitHub with them.
async fn ways(State(e): S) -> Response {
    run(move || Ok(json!({ "accounts": on(&e.db.lock().unwrap())?, "github": github_app().is_some(), "signup": super::orgs::signup_open(), "password": super::orgs::on() }))).await
}

/// The server's GitHub OAuth app, from its environment: `AVP_GITHUB_CLIENT_ID` and
/// `AVP_GITHUB_CLIENT_SECRET`.
fn github_app() -> Option<(String, String)> {
    let v = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    Some((v("AVP_GITHUB_CLIENT_ID")?, v("AVP_GITHUB_CLIENT_SECRET")?))
}

/// GitHub's web side (`AVP_GITHUB_URL`) and its API (`AVP_GITHUB_API`, as the bridge's): github.com
/// unless a GitHub Enterprise says otherwise.
fn github_urls() -> (String, String) {
    let v = |k: &str, d: &str| std::env::var(k).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| d.into()).trim_end_matches('/').to_string();
    (v("AVP_GITHUB_URL", "https://github.com"), v("AVP_GITHUB_API", "https://api.github.com"))
}

/// Back to AVP Live, with what went wrong for its sign-in to say.
fn back_to_live(why: Option<&str>) -> Response {
    let mut u = reqwest::Url::parse("http://avp/live").unwrap();
    if let Some(why) = why {
        u.query_pairs_mut().append_pair("signin", why);
    }
    let to = match u.query() {
        Some(q) => format!("/live?{q}"),
        None => "/live".into(),
    };
    (StatusCode::FOUND, [("location", to)]).into_response()
}

async fn github_start(headers: HeaderMap) -> Response {
    let Some((client_id, _)) = github_app() else { return back_to_live(Some("GitHub sign-in is off on this server: AVP_GITHUB_CLIENT_ID and AVP_GITHUB_CLIENT_SECRET turn it on")) };
    let Ok(state) = fresh() else { return back_to_live(Some("the server could not draw a random state")) };
    let mut u = reqwest::Url::parse(&format!("{}/login/oauth/authorize", github_urls().0)).unwrap();
    u.query_pairs_mut().append_pair("client_id", &client_id).append_pair("state", &state).append_pair("allow_signup", if super::orgs::signup_open() { "true" } else { "false" });
    let mut res = (StatusCode::FOUND, [("location", u.to_string())]).into_response();
    // Lax: GitHub's redirect back is a navigation from another site, which a Strict cookie misses.
    if let Ok(v) = axum::http::HeaderValue::from_str(&set_cookie(&headers, OAUTH, &state, 600, "Lax")) {
        res.headers_mut().append("set-cookie", v);
    }
    res
}

#[derive(Deserialize)]
struct Back {
    code: Option<String>,
    state: Option<String>,
}

/// GitHub's answer: the code becomes the login of who signed in there, and the login a person here.
async fn github_back(State(e): S, Extension(known): Extension<Known>, ext: Extensions, headers: HeaderMap, Query(q): Query<Back>) -> Response {
    let Some((client_id, secret)) = github_app() else { return back_to_live(Some("GitHub sign-in is off on this server")) };
    let (Some(code), Some(state)) = (q.code, q.state) else { return back_to_live(Some("GitHub sent no code back: sign in again")) };
    if cookie(&headers, OAUTH).as_deref() != Some(state.as_str()) {
        return back_to_live(Some("this sign-in did not start here, or took more than 10 minutes: sign in again"));
    }
    let from = address(&headers, &ext);
    let out = tokio::task::spawn_blocking(move || -> Result<std::result::Result<NewToken, String>> {
        let login = github_login(&github_user(&client_id, &secret, &code)?);
        let db = e.db.lock().unwrap();
        match user_of_github(&db, &login)? {
            Some(u) => Ok(Ok(session(&db, &u.name, "github")?)),
            // AVP Cloud open to sign-ups: the GitHub account makes the person, named as on GitHub.
            None if super::orgs::signup_open() => {
                if !within_limit("signup", &from, SIGNUPS_AN_HOUR, 3_600_000) {
                    return Ok(Err("too many sign-ups from this address: try again later".into()));
                }
                let made = add_person(&db, &login, None, Some(&login), false, "sign-up");
                known.forget();
                match made {
                    Ok((u, _)) => Ok(Ok(session(&db, &u.name, "github")?)),
                    Err(x) => Ok(Err(format!("{x:#}: sign up with an email and another name, then ask a server admin to link {login}"))),
                }
            }
            None => Ok(Err(format!("the GitHub account {login} is no person of this server: a server admin adds you with `avp user add <name> --github {login}`"))),
        }
    })
    .await;
    let mut res = match out {
        Ok(Ok(Ok(s))) => signed_in(&headers, &s, back_to_live(None)),
        Ok(Ok(Err(why))) => back_to_live(Some(&why)),
        Ok(Err(e)) => back_to_live(Some(&format!("GitHub: {e:#}"))),
        Err(e) => back_to_live(Some(&e.to_string())),
    };
    if let Ok(v) = axum::http::HeaderValue::from_str(&set_cookie(&headers, OAUTH, "", 0, "Lax")) {
        res.headers_mut().append("set-cookie", v);
    }
    res
}

/// The GitHub login a sign-in's code stands for: the code traded for a token of the app's, the
/// token asked whose it is. The token is used once and kept nowhere.
fn github_user(client_id: &str, secret: &str, code: &str) -> Result<String> {
    let (web, api) = github_urls();
    let http = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(15)).build()?;
    let got: serde_json::Value = http
        .post(format!("{web}/login/oauth/access_token"))
        .header("accept", "application/json")
        .json(&json!({ "client_id": client_id, "client_secret": secret, "code": code }))
        .send()?
        .json()?;
    let access = got["access_token"].as_str().ok_or_else(|| anyhow!("{}", got["error_description"].as_str().or(got["error"].as_str()).unwrap_or("no access token")))?;
    let me: serde_json::Value = http.get(format!("{api}/user")).header("accept", "application/vnd.github+json").header("user-agent", "avp").bearer_auth(access).send()?.error_for_status()?.json()?;
    me["login"].as_str().map(String::from).ok_or_else(|| anyhow!("GitHub said no login"))
}

// ---- `avp login`: device codes ----

/// How long a device code waits for a person's yes, and how often the CLI asks meanwhile.
const DEVICE_SECS: i64 = 600;
const DEVICE_POLL_SECS: i64 = 2;
/// The letters of the code a person checks in the browser: no vowels (no word spelled by chance),
/// none read as another.
const CODE_LETTERS: &[u8] = b"BCDFGHJKLMNPQRSTVWXZ";

/// A device code waiting for its person: kept in memory, since it lives minutes (a restart drops
/// it, and `avp login` asks for another).
struct Device {
    user_code: String,
    /// The machine that asks, for the person to recognize it, and as the token's name.
    name: String,
    expires_at: i64,
    /// Whose yes it got (Ok), or that it got a no (Err).
    decided: Option<Result<String, String>>,
}

/// The device codes waiting, by the hash of their secret half: what the CLI holds is nowhere here.
fn devices() -> std::sync::MutexGuard<'static, std::collections::HashMap<String, Device>> {
    static DEVICES: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Device>>> = std::sync::OnceLock::new();
    let mut d = DEVICES.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    let now = now_ms();
    d.retain(|_, x| x.expires_at > now);
    d
}

/// What `avp login` gets to start: the secret it polls with, the code the person checks.
#[derive(Debug, Serialize)]
pub struct DeviceStart {
    pub device_code: String,
    pub user_code: String,
    pub expires_in: i64,
    pub interval: i64,
}

/// Starts a device login for the machine `name`.
pub fn device_start(name: &str) -> Result<DeviceStart> {
    use chacha20poly1305::aead::{Generate, Key};
    let device_code = fresh()?;
    let bytes: [u8; 32] = Key::<chacha20poly1305::XChaCha20Poly1305>::try_generate().map_err(|e| anyhow!("the system's random generator: {e}"))?.into();
    // 20^8 codes: guessing one in its ten minutes, signed in, is out of reach.
    let letters: String = bytes[..8].iter().map(|b| CODE_LETTERS[*b as usize % CODE_LETTERS.len()] as char).collect();
    let user_code = format!("{}-{}", &letters[..4], &letters[4..]);
    let name: String = name.trim().chars().filter(|c| !c.is_control()).take(64).collect();
    let name = if name.is_empty() { "a machine".to_string() } else { name };
    let mut d = devices();
    // Anyone may start one: a flood waits its ten minutes out instead of filling the memory.
    anyhow::ensure!(d.len() < 10_000, "too many logins wait right now: try again in a few minutes");
    d.insert(hash(&device_code), Device { user_code: user_code.clone(), name, expires_at: now_ms() + DEVICE_SECS * 1000, decided: None });
    Ok(DeviceStart { device_code, user_code, expires_in: DEVICE_SECS, interval: DEVICE_POLL_SECS })
}

/// A code as a person types it: case, spaces and the dash do not matter.
fn user_code_of(typed: &str) -> String {
    let l: String = typed.chars().filter(|c| c.is_ascii_alphabetic()).map(|c| c.to_ascii_uppercase()).collect();
    if l.len() == 8 { format!("{}-{}", &l[..4], &l[4..]) } else { l }
}

/// The machine a code stands for, for the page to name before the person says yes.
pub fn device_named(typed: &str) -> Option<String> {
    let code = user_code_of(typed);
    devices().values().find(|d| d.user_code == code && d.decided.is_none()).map(|d| d.name.clone())
}

/// A person's yes (or no) on a code: once, while it waits. Gives the machine's name.
pub fn device_decide(typed: &str, person: &str, yes: bool) -> Result<String> {
    let code = user_code_of(typed);
    let mut d = devices();
    let dev = d.values_mut().find(|d| d.user_code == code).ok_or_else(|| anyhow!("no login waits with the code {code}: it ended (10 minutes), or was mistyped. Run `avp login` again"))?;
    anyhow::ensure!(dev.decided.is_none(), "the code {code} was already answered: run `avp login` again");
    dev.decided = Some(if yes { Ok(person.to_string()) } else { Err(person.to_string()) });
    Ok(dev.name.clone())
}

/// What the CLI's poll finds.
#[derive(Debug)]
pub enum Polled {
    Pending,
    /// A person's yes: their token, made now, the code gone (single use).
    Approved(NewToken),
    Denied(String),
    /// Ended, used, or never made.
    Gone,
}

pub fn device_poll(db: &Db, device_code: &str) -> Result<Polled> {
    let key = hash(device_code.trim());
    let mut d = devices();
    let Some(dev) = d.get(&key) else { return Ok(Polled::Gone) };
    match dev.decided.clone() {
        None => Ok(Polled::Pending),
        Some(Err(by)) => {
            d.remove(&key);
            Ok(Polled::Denied(by))
        }
        Some(Ok(person)) => {
            let name = format!("avp login ({})", dev.name);
            d.remove(&key);
            drop(d);
            anyhow::ensure!(user(db, &person)?.is_some(), "{person} is no longer a person of this server");
            Ok(Polled::Approved(create_token(db, &person, &name, "token", None, &person)?))
        }
    }
}

/// The server's address as people reach it: `AVP_PUBLIC_URL`, else what the request came to,
/// through the proxy in front. Only the characters of a host and port: it goes into a script.
pub fn public_url(h: &HeaderMap) -> String {
    if let Some(u) = std::env::var("AVP_PUBLIC_URL").ok().map(|u| u.trim().trim_end_matches('/').to_string()).filter(|u| !u.is_empty()) {
        return u;
    }
    let v = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(|v| v.split(',').next().unwrap_or("").trim().to_string()).filter(|v| !v.is_empty());
    let host = v("x-forwarded-host").or_else(|| v("host")).filter(|x| x.chars().all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))).unwrap_or_else(|| "127.0.0.1:7117".into());
    let https = v("x-forwarded-proto").is_some_and(|p| p.eq_ignore_ascii_case("https"));
    format!("{}://{host}", if https { "https" } else { "http" })
}

#[derive(Deserialize)]
struct DeviceAsk {
    name: Option<String>,
}

async fn device_create(State(e): S, headers: HeaderMap, Json(b): Json<DeviceAsk>) -> Response {
    let name = b.name.unwrap_or_default();
    let url = public_url(&headers);
    run(move || {
        anyhow::ensure!(on(&e.db.lock().unwrap())?, "this server has no accounts: there is nothing to log in to, AVP_SERVER is enough");
        let s = device_start(&name)?;
        let page = format!("{url}/login/device");
        Ok(json!({ "device_code": s.device_code, "user_code": s.user_code, "verification_uri": page, "verification_uri_complete": format!("{page}?code={}", s.user_code), "expires_in": s.expires_in, "interval": s.interval }))
    })
    .await
}

#[derive(Deserialize)]
struct DeviceAnswer {
    code: String,
    #[serde(default)]
    deny: bool,
}

async fn device_approve(Extension(who): Extension<Who>, Json(b): Json<DeviceAnswer>) -> Response {
    run(move || {
        let Who::Person(u) = &who else { anyhow::bail!(Forbidden("a login is approved by a person: sign in with your own account".into())) };
        let name = device_decide(&b.code, &u.name, !b.deny)?;
        Ok(json!({ "person": u.name, "machine": name, "approved": !b.deny }))
    })
    .await
}

#[derive(Deserialize)]
struct DeviceCode {
    #[serde(default)]
    code: Option<String>,
}

/// What the page shows before the yes: the machine that asks.
async fn device_peek(Query(q): Query<DeviceCode>) -> Response {
    let machine = q.code.as_deref().and_then(device_named);
    Json(json!({ "machine": machine })).into_response()
}

#[derive(Deserialize)]
struct DevicePoll {
    device_code: String,
}

async fn device_token(State(e): S, Json(b): Json<DevicePoll>) -> Response {
    let out = tokio::task::spawn_blocking(move || device_poll(&e.db.lock().unwrap(), &b.device_code)).await;
    match out {
        Ok(Ok(Polled::Pending)) => Json(json!({ "status": "pending" })).into_response(),
        Ok(Ok(Polled::Approved(t))) => Json(json!({ "status": "approved", "person": t.info.person, "token": t })).into_response(),
        Ok(Ok(Polled::Denied(by))) => json_refusal(StatusCode::FORBIDDEN, format!("{by} said no to this login")),
        Ok(Ok(Polled::Gone)) => json_refusal(StatusCode::GONE, "this login ended (10 minutes) or was used: run `avp login` again"),
        Ok(Err(e)) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")),
        Err(e) => json_refusal(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

/// The page a person approves a login on, signed in to this server.
async fn device_page() -> Response {
    ([(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")], DEVICE_PAGE).into_response()
}

/// `curl -fsSL <server>/install | sh`: install.sh, with this server as the one it logs in to.
async fn install_script(headers: HeaderMap) -> Response {
    let url = public_url(&headers);
    let script = include_str!("../../install.sh").replacen("cloud=${AVP_CLOUD_URL:-}", &format!("cloud=${{AVP_CLOUD_URL:-{url}}}"), 1);
    ([(axum::http::header::CONTENT_TYPE, "text/x-shellscript; charset=utf-8")], script).into_response()
}

const DEVICE_PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#000000">
<title>Log in a machine - AVP</title>
<style>
:root { --bg: #000; --card: #0c0c0c; --line: #232323; --fg: #ececec; --act: #fff; --mut: #8d8d8d; --green: #3ccf74; --red: #ff5a52; --ease: cubic-bezier(.2, .8, .2, 1); }
* { box-sizing: border-box; }
html, body { margin: 0; min-height: 100%; background: var(--bg); color: var(--fg); }
body { font: 15px/1.5 -apple-system, BlinkMacSystemFont, "Inter", "Segoe UI", system-ui, sans-serif; -webkit-font-smoothing: antialiased; display: grid; place-items: center; min-height: 100vh; padding: 24px; }
@keyframes rise { from { opacity: 0; transform: translateY(8px); } to { opacity: 1; transform: none; } }
.card { width: 100%; max-width: 420px; background: var(--card); border: 1px solid var(--line); border-radius: 18px; padding: 32px 28px; animation: rise .45s var(--ease); }
h1 { font-size: 24px; font-weight: 680; letter-spacing: -.02em; margin: 0 0 6px; }
p { color: var(--mut); margin: 0 0 22px; }
p b { color: var(--fg); font-weight: 580; }
input { width: 100%; font: 600 26px/1 ui-monospace, "SF Mono", Menlo, monospace; letter-spacing: .14em; text-align: center; text-transform: uppercase; color: var(--act); background: #000; border: 1px solid var(--line); border-radius: 12px; padding: 16px 12px; outline: none; transition: border-color .2s; }
input:focus { border-color: var(--mut); }
input.small { font: 14px/1.4 ui-monospace, "SF Mono", Menlo, monospace; letter-spacing: 0; text-transform: none; text-align: left; padding: 12px; }
.row { display: flex; gap: 10px; margin-top: 18px; }
button { flex: 1; font: inherit; font-weight: 600; border-radius: 12px; padding: 12px; cursor: pointer; border: 1px solid var(--line); background: transparent; color: var(--fg); transition: transform .12s var(--ease), background .2s; }
button.go { background: var(--act); color: #000; border-color: var(--act); }
button:active { transform: scale(.97); }
button:disabled { opacity: .5; cursor: default; }
.said { margin-top: 18px; min-height: 1.5em; color: var(--mut); }
.said.ok { color: var(--green); }
.said.no { color: var(--red); }
[hidden] { display: none !important; }
</style>
</head>
<body>
<main class="card">
  <section id="signin" hidden>
    <h1>Sign in first</h1>
    <p>A machine asks to work as you on this server. Sign in, then approve it here.</p>
    <input id="token" class="small" placeholder="your token (avp_...)" autocomplete="off" spellcheck="false">
    <div class="row"><button class="go" id="in">Sign in</button></div>
    <p class="said" id="other"></p>
  </section>
  <section id="approve" hidden>
    <h1>Log in a machine</h1>
    <p id="as">Check that the code is the one your terminal shows.</p>
    <input id="code" maxlength="9" autocomplete="off" spellcheck="false" placeholder="XXXX-XXXX">
    <div class="row"><button id="deny">Deny</button><button class="go" id="yes">Approve</button></div>
  </section>
  <div class="said" id="said" aria-live="polite"></div>
</main>
<script>
const $ = (id) => document.getElementById(id);
const live = { "x-avp-live": "1", "content-type": "application/json" };
const say = (text, kind) => { $("said").textContent = text; $("said").className = "said" + (kind ? " " + kind : ""); };
const code = new URLSearchParams(location.search).get("code") || "";
$("code").value = code;

async function peek() {
  const c = $("code").value.trim();
  if (!c) return;
  const r = await fetch("/login/device/peek?code=" + encodeURIComponent(c));
  const m = (await r.json()).machine;
  say(m ? "Asked by " + m + "." : "No login waits with this code: it ended or was mistyped.", m ? "" : "no");
}

async function start() {
  const r = await fetch("/me", { headers: live });
  const me = r.ok ? await r.json() : null;
  if (me && me.via === "token" && me.person) {
    $("approve").hidden = false;
    $("as").innerHTML = "Signed in as <b></b>. Check that the code is the one your terminal shows.";
    $("as").querySelector("b").textContent = me.person;
    peek();
  } else {
    $("signin").hidden = false;
    $("other").innerHTML = 'Or sign in another way on <a href="/live" style="color:inherit;text-decoration:underline">AVP Live</a>, then come back to this page.';
  }
}

$("in").onclick = async () => {
  const r = await fetch("/session", { method: "POST", headers: live, body: JSON.stringify({ token: $("token").value.trim() }) });
  if (!r.ok) return say((await r.json().catch(() => ({}))).error || "That token does not sign in here.", "no");
  $("signin").hidden = true; say(""); start();
};

async function answer(deny) {
  $("yes").disabled = $("deny").disabled = true;
  const r = await fetch("/login/device/approve", { method: "POST", headers: live, body: JSON.stringify({ code: $("code").value, deny }) });
  const b = await r.json().catch(() => ({}));
  if (!r.ok) { $("yes").disabled = $("deny").disabled = false; return say(b.error || "That did not work.", "no"); }
  $("approve").hidden = true;
  say(deny ? "Denied: " + b.machine + " stays logged out." : b.machine + " is logged in as " + b.person + ". Back to your terminal.", deny ? "no" : "ok");
}
$("yes").onclick = () => answer(false);
$("deny").onclick = () => answer(true);
$("code").onchange = peek;
start();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_need_the_role_their_verb_does() {
        let need = |m: Method, p: &str| need(&m, p);
        let repo = |r: Role| Need::Repo("web".into(), r);
        assert_eq!(need(Method::GET, "/repos/web/line"), repo(Role::Read));
        assert_eq!(need(Method::POST, "/repos/web/objects"), repo(Role::Read), "a batch of objects is read with a POST");
        assert_eq!(need(Method::POST, "/repos/web/tasks"), repo(Role::Write));
        assert_eq!(need(Method::POST, "/repos/web/tasks/t1/land"), repo(Role::Write));
        assert_eq!(need(Method::PUT, "/repos/web/objects/abc"), repo(Role::Write));
        assert_eq!(need(Method::POST, "/repos/web/guarded"), repo(Role::Admin));
        assert_eq!(need(Method::GET, "/repos/web/guarded"), repo(Role::Read));
        assert_eq!(need(Method::POST, "/repos/web/landing"), repo(Role::Admin));
        assert_eq!(need(Method::GET, "/repos/web/secrets"), repo(Role::Admin), "even the names of secrets");
        assert_eq!(need(Method::POST, "/repos/web/envs/production/observed"), repo(Role::Admin));
        assert_eq!(need(Method::POST, "/repos/web/envs/production"), Need::Promote("web".into(), "production".into()));
        assert_eq!(need(Method::POST, "/repos/web/envs/production/rollback"), Need::Promote("web".into(), "production".into()));
        assert_eq!(need(Method::POST, "/work/claim"), Need::ServerAdmin, "a job carries secrets");
        assert_eq!(need(Method::GET, "/work"), Need::Person);
        assert_eq!(need(Method::POST, "/users"), Need::ServerAdmin);
        assert_eq!(need(Method::GET, "/live"), Need::Nothing);
        assert_eq!(need(Method::POST, "/repos"), Need::Person, "a person creates a repo, and is its admin");
    }

    #[test]
    fn a_token_is_kept_hashed_and_stops_when_revoked_or_expired() {
        let dir = std::env::temp_dir().join(format!("avp-auth-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("avp.sqlite")).unwrap();
        assert!(!on(&db).unwrap());
        let (ada, first) = add_user(&db, "ada", Some("ada@example.com"), Some("@Ada-L"), false, "server").unwrap();
        assert!(ada.admin && on(&db).unwrap(), "the first person turns accounts on and is an admin");
        let (bob, _) = add_user(&db, "bob", None, None, false, "ada").unwrap();
        assert!(!bob.admin, "the next ones are not");
        assert!(first.token.starts_with("avp_") && first.token.len() > 40, "{}", first.token);
        let kept: String = db.conn.query_row("SELECT hash FROM tokens WHERE id = ?1", params![first.info.id], |r| r.get(0)).unwrap();
        assert_eq!(kept, hash(&first.token));
        let all: String = db.conn.query_row("SELECT group_concat(hash || person || name) FROM tokens", [], |r| r.get(0)).unwrap();
        assert!(!all.contains(&first.token), "the value is nowhere in the database");
        assert!(matches!(check_token(&db, &first.token).unwrap(), Checked::Person(u) if u.name == "ada"));
        assert!(matches!(check_token(&db, "avp_guessed").unwrap(), Checked::Unknown));
        assert_eq!(user_of_github(&db, "ada-l").unwrap().map(|u| u.name).as_deref(), Some("ada"), "a GitHub login, whatever its case");

        let t = create_token(&db, "bob", "laptop", "token", None, "bob").unwrap();
        revoke(&db, &t.info.id, "bob").unwrap();
        assert!(matches!(check_token(&db, &t.token).unwrap(), Checked::Revoked));
        let old = create_token(&db, "bob", "old", "token", Some(now_ms() - 1), "bob").unwrap();
        assert!(matches!(check_token(&db, &old.token).unwrap(), Checked::Expired(_)));
        let other = create_token(&db, "bob", "other", "token", expiry("30d", now_ms()).unwrap(), "bob").unwrap();
        remove_user(&db, "bob", "ada").unwrap();
        assert!(matches!(check_token(&db, &other.token).unwrap(), Checked::Revoked), "a person removed: their tokens stop");
        let kinds: Vec<String> = account_events(&db, 0, 100).unwrap().into_iter().map(|e| format!("{} by {}", e.kind, e.data["by"].as_str().unwrap())).collect();
        assert!(kinds.contains(&"user.added by server".into()) && kinds.contains(&"token.revoked by bob".into()) && kinds.contains(&"user.removed by ada".into()), "{kinds:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_device_login_gives_its_token_once_to_the_person_who_approved_it() {
        let dir = std::env::temp_dir().join(format!("avp-device-{}", new_id("t")));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("avp.sqlite")).unwrap();
        add_user(&db, "ada", None, None, false, "server").unwrap();
        let s = device_start("ada on laptop").unwrap();
        assert!(s.user_code.len() == 9 && s.user_code.as_bytes()[4] == b'-', "{}", s.user_code);
        assert!(matches!(device_poll(&db, &s.device_code).unwrap(), Polled::Pending));
        assert_eq!(device_named(&s.user_code.to_lowercase().replace('-', " ")).as_deref(), Some("ada on laptop"), "typed in any case, any spacing");
        assert!(device_decide("BBBB-BBBB", "ada", true).is_err(), "a code that waits for nothing");

        assert_eq!(device_decide(&s.user_code, "ada", true).unwrap(), "ada on laptop");
        assert!(device_decide(&s.user_code, "ada", true).is_err(), "answered once");
        let Polled::Approved(t) = device_poll(&db, &s.device_code).unwrap() else { panic!("approved") };
        assert_eq!((t.info.person.as_str(), t.info.name.as_str()), ("ada", "avp login (ada on laptop)"));
        assert!(matches!(check_token(&db, &t.token).unwrap(), Checked::Person(u) if u.name == "ada"));
        assert!(matches!(device_poll(&db, &s.device_code).unwrap(), Polled::Gone), "single use");

        let no = device_start("someone else").unwrap();
        device_decide(&no.user_code, "ada", false).unwrap();
        assert!(matches!(device_poll(&db, &no.device_code).unwrap(), Polled::Denied(by) if by == "ada"));
        assert!(matches!(device_poll(&db, &no.device_code).unwrap(), Polled::Gone));

        let late = device_start("slow").unwrap();
        devices().get_mut(&hash(&late.device_code)).unwrap().expires_at = now_ms() - 1;
        assert!(matches!(device_poll(&db, &late.device_code).unwrap(), Polled::Gone), "short-lived");
        assert!(device_decide(&late.user_code, "ada", true).is_err());
        assert_eq!(need(&Method::POST, "/login/device/approve"), Need::Person, "approved by a signed-in person");
        assert_eq!(need(&Method::POST, "/login/device/token"), Need::Nothing, "polled before any token");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_token_ends_after_a_span_or_on_a_date() {
        let now = crate::stats::parse_time("2026-10-28T12:00:00Z").unwrap();
        assert_eq!(expiry("never", now).unwrap(), None);
        assert_eq!(expiry("90d", now).unwrap(), Some(now + 90 * 86_400_000));
        assert_eq!(expiry("2027-01-01", now).unwrap(), Some(crate::stats::parse_time("2027-01-01").unwrap()));
        assert!(expiry("2026-01-01", now).is_err(), "a date gone by");
    }
}
