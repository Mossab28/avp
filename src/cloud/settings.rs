//! A repo's settings, as its admins change them from the settings page, `avp repo` or the avp_repo
//! tool, all through these routes: who reaches it (people, its organization), the tokens scoped to
//! it for an agent or a CI, its name (an old one keeps answering), description and visibility, its
//! space, archive and deletion (kept 7 days, restorable), and what guards it. Every change is an
//! event of the repo (`repo.*`, `role.set`): its audit log. The gate (auth.rs) enforces what they
//! set: old names, public reads, a token's scope, an archived or deleted repo.

use super::auth::{self, Forbidden, Role, Who};
use super::orgs;
use crate::db::Db;
use crate::engine::{Engine, CHECK_FILES};
use crate::model::{now_ms, Repo};
use anyhow::{anyhow, Result};
use axum::extract::{Path as P, Query, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// How long a deleted repo is kept, restorable by an admin of it, before its data goes.
pub const KEEP_DAYS: i64 = 7;
const DAY_MS: i64 = 86_400_000;
/// Who reads a public repo signed out: no person's name (a name has no space nor parenthesis).
pub(crate) const GUEST: &str = "(signed out)";

pub(crate) fn tables(db: &Db) -> Result<()> {
    db.conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS repo_settings (
           repo TEXT PRIMARY KEY, description TEXT NOT NULL DEFAULT '', visibility TEXT NOT NULL DEFAULT 'private',
           archived_at INTEGER, archived_by TEXT, deleted_at INTEGER, deleted_by TEXT
         );
         CREATE TABLE IF NOT EXISTS repo_names (
           space TEXT NOT NULL, name TEXT NOT NULL, repo TEXT NOT NULL, at INTEGER NOT NULL, PRIMARY KEY (space, name)
         );
         CREATE TABLE IF NOT EXISTS repo_tokens (token TEXT PRIMARY KEY, repo TEXT NOT NULL, role TEXT NOT NULL);
         CREATE INDEX IF NOT EXISTS repo_tokens_repo ON repo_tokens (repo);",
    )?;
    Ok(())
}

/// What the settings add to a repo; a repo never changed is private, live, with no description.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Settings {
    pub description: String,
    pub visibility: String,
    pub archived_at: Option<i64>,
    pub archived_by: Option<String>,
    pub deleted_at: Option<i64>,
    pub deleted_by: Option<String>,
}

pub fn settings(db: &Db, repo: &str) -> Result<Settings> {
    let s = db
        .conn
        .query_row("SELECT description, visibility, archived_at, archived_by, deleted_at, deleted_by FROM repo_settings WHERE repo = ?1", params![repo], |r| {
            Ok(Settings { description: r.get(0)?, visibility: r.get(1)?, archived_at: r.get(2)?, archived_by: r.get(3)?, deleted_at: r.get(4)?, deleted_by: r.get(5)? })
        })
        .optional()?;
    Ok(s.unwrap_or_else(|| Settings { visibility: "private".into(), ..Default::default() }))
}

fn set(db: &Db, repo: &str, column: &str, value: rusqlite::types::Value) -> Result<()> {
    db.conn.execute("INSERT OR IGNORE INTO repo_settings (repo) VALUES (?1)", params![repo])?;
    db.conn.execute(&format!("UPDATE repo_settings SET {column} = ?2 WHERE repo = ?1"), params![repo, value])?;
    Ok(())
}

/// Anyone reads it, signed in or not; nobody writes without a role.
/// Public repos are the server's choice (`AVP_PUBLIC_REPOS=1`): off, none is, whatever was set.
pub fn public_allowed() -> bool {
    std::env::var("AVP_PUBLIC_REPOS").is_ok_and(|v| v.trim() == "1")
}

pub fn public(db: &Db, repo: &str) -> Result<bool> {
    Ok(public_allowed() && settings(db, repo)?.visibility == "public")
}

/// When a deleted repo's data goes.
pub(crate) fn purge_at(deleted_at: i64) -> i64 {
    deleted_at + KEEP_DAYS * DAY_MS
}

// ---- what the gate reads ----

/// Whether a key (an id, `<space>/<name>`, a name) names a repo as it is now.
fn known(db: &Db, key: &str) -> Result<bool> {
    Ok(db.conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM repos WHERE id = ?1 OR name = ?1 OR COALESCE(org, owner, '') || '/' || name = ?1)",
        params![key],
        |r| r.get(0),
    )?)
}

/// The repo a name it had before names: `<space>/<name>`, or a name alone that one repo had.
pub fn renamed(db: &Db, key: &str) -> Result<Option<String>> {
    let (space, name) = match key.split_once('/') {
        Some((s, n)) => (Some(s), n),
        None => (None, key),
    };
    let mut st = db.conn.prepare("SELECT repo FROM repo_names WHERE (?1 IS NULL OR space = ?1) AND name = ?2 AND repo IN (SELECT id FROM repos)")?;
    let ids = st.query_map(params![space, name], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(match ids.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    })
}

/// A path that names a repo by a name it had (renamed, or transferred to another space), with the
/// repo said by its id: the old URL, `git clone` included, keeps reaching it. None: as it is.
pub fn aliased(db: &Db, path: &str) -> Result<Option<String>> {
    let trimmed = path.trim_start_matches('/');
    if let Some((key, rest)) = trimmed.split_once(".git/") {
        if known(db, key)? {
            return Ok(None);
        }
        return Ok(renamed(db, key)?.map(|id| format!("/{id}.git/{rest}")));
    }
    let seg: Vec<&str> = trimmed.split('/').collect();
    let ["repos", first, after @ ..] = seg.as_slice() else { return Ok(None) };
    let first = first.replace("%2F", "/").replace("%2f", "/");
    let to = |id: String, after: &[&str]| format!("/repos/{id}{}", after.iter().map(|s| format!("/{s}")).collect::<String>());
    if known(db, &first)? {
        return Ok(None);
    }
    if let (false, [name, rest @ ..]) = (first.contains('/'), after) {
        let two = format!("{first}/{name}");
        if known(db, &two)? {
            return Ok(None);
        }
        if let Some(id) = renamed(db, &two)? {
            return Ok(Some(to(id, rest)));
        }
    }
    Ok(renamed(db, &first)?.map(|id| to(id, after)))
}

/// A token made for one repo: which, and the most it does there.
#[derive(Clone, Debug)]
pub struct Scope {
    pub repo: String,
    pub role: Role,
}

pub fn scope(db: &Db, token: &str) -> Result<Option<Scope>> {
    let row: Option<(String, String)> = db
        .conn
        .query_row("SELECT s.repo, s.role FROM repo_tokens s JOIN tokens t ON t.id = s.token WHERE t.hash = ?1", params![auth::hash(token)], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    Ok(row.and_then(|(repo, role)| Some(Scope { repo, role: Role::parse(&role).ok()?? })))
}

/// The repo a token was made for, by its name, revoked or not. None: a person's own token.
pub fn made_for(db: &Db, token: &str) -> Result<Option<String>> {
    let repo: Option<String> = db.conn.query_row("SELECT s.repo FROM repo_tokens s JOIN tokens t ON t.id = s.token WHERE t.hash = ?1", params![auth::hash(token)], |r| r.get(0)).optional()?;
    Ok(repo.and_then(|r| db.repo(&r).ok()).map(|r| r.full_name()))
}

/// The repo a request's path names, by the key it uses.
fn repo_key(path: &str) -> Option<(String, Vec<String>)> {
    let trimmed = path.trim_start_matches('/');
    if let Some((key, rest)) = trimmed.split_once(".git/") {
        return Some((key.to_string(), vec![".git".into(), rest.to_string()]));
    }
    let seg: Vec<&str> = trimmed.split('/').collect();
    match seg.as_slice() {
        ["repos", key, rest @ ..] => Some((key.replace("%2F", "/").replace("%2f", "/"), rest.iter().map(|s| s.to_string()).collect())),
        _ => None,
    }
}

/// What an archived or a deleted repo refuses, whoever asks: an archived one takes no change but its
/// settings' (it reads as before); a deleted one answers as missing, but to its admins' settings
/// page, its log and its restore. None: the route goes on.
pub fn closed(db: &Db, method: &Method, path: &str) -> Result<Option<(StatusCode, String)>> {
    let Some((key, rest)) = repo_key(path) else { return Ok(None) };
    let Ok(repo) = db.repo(&key) else { return Ok(None) };
    let s = settings(db, &repo.id)?;
    let name = repo.full_name();
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    let read = *method == Method::GET || *method == Method::HEAD;
    if let Some(at) = s.deleted_at {
        let kept = matches!(rest.as_slice(), ["settings"] | ["settings", "audit"] if read) || matches!(rest.as_slice(), ["settings", "restore"]);
        if !kept {
            return Ok(Some((
                StatusCode::NOT_FOUND,
                format!(
                    "no repo {name}: it was deleted on {} UTC{}. An admin of it restores it until {} UTC: `avp repo restore --repo {name}`",
                    crate::blame::utc(at),
                    s.deleted_by.map(|b| format!(" by {b}")).unwrap_or_default(),
                    crate::blame::utc(purge_at(at))
                ),
            )));
        }
    }
    if let Some(at) = s.archived_at {
        let push = rest.last().is_some_and(|r| r.ends_with("git-receive-pack"));
        let reads = (read && !push) || matches!(rest.as_slice(), ["objects"] | ["diff"] | ["history", "context"] | [".git", "git-upload-pack"]);
        if !reads && rest.first() != Some(&"settings") {
            return Ok(Some((
                StatusCode::FORBIDDEN,
                format!(
                    "{name} is archived (read-only since {} UTC{}): nothing opens nor lands on it. An admin of it unarchives it: `avp repo archive --off --repo {name}`",
                    crate::blame::utc(at),
                    s.archived_by.map(|b| format!(", by {b}")).unwrap_or_default()
                ),
            )));
        }
    }
    Ok(None)
}

/// The repos a list shows: not those deleted.
pub fn live(db: &Db, repos: Vec<Repo>) -> Result<Vec<Repo>> {
    let mut st = db.conn.prepare("SELECT repo FROM repo_settings WHERE deleted_at IS NOT NULL")?;
    let gone: std::collections::HashSet<String> = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    Ok(repos.into_iter().filter(|r| !gone.contains(&r.id)).collect())
}

// ---- the changes ----

/// The repo, for an admin of it (a server admin, the server's key and its machine are).
fn administered(db: &Db, key: &str, who: &Who) -> Result<Repo> {
    let r = db.repo(key)?;
    if let Who::Person(u) = who {
        anyhow::ensure!(
            u.admin || auth::role(db, &r.id, &u.name)? == Some(Role::Admin),
            Forbidden(format!("{} is no admin of {}: its settings are its admins'", u.name, r.full_name()))
        );
    }
    Ok(r)
}

fn role_name(r: Option<Role>) -> &'static str {
    r.map(Role::as_str).unwrap_or("none")
}

/// Who reaches the repo: the people given a role on it, and its organization's members by their org
/// role (a role on the repo comes first).
pub fn collaborators(db: &Db, repo: &Repo) -> Result<Vec<Value>> {
    let own = auth::roles_of_repo(db, &repo.id)?;
    let mut out: Vec<Value> = own.iter().map(|(p, r)| json!({ "person": p, "role": r, "via": "repo" })).collect();
    if let Some(org) = &repo.org {
        for (p, r) in orgs::members(db, org)? {
            match out.iter_mut().find(|c| c["person"] == p.as_str()) {
                Some(c) => c["org_role"] = json!(r),
                None => out.push(json!({ "person": p, "role": r.repo_role(), "via": "org", "org_role": r })),
            }
        }
    }
    out.sort_by(|a, b| a["person"].as_str().cmp(&b["person"].as_str()));
    Ok(out)
}

/// The person a name or an email is, on this server.
fn person_of(db: &Db, to: &str) -> Result<String> {
    let to = to.trim();
    if to.contains('@') {
        let found: Option<String> = db.conn.query_row("SELECT name FROM users WHERE lower(email) = lower(?1)", params![to], |r| r.get(0)).optional()?;
        return found.ok_or_else(|| anyhow!("no one signed up with {to} on this server: once they have, invite them again, they get the role at once"));
    }
    anyhow::ensure!(auth::user(db, to)?.is_some(), "no person {to} on this server: once they signed up, invite them again (or by email)");
    Ok(to.to_string())
}

/// Gives a person (by name or by email) a role on the repo at once, changes it, or with None takes
/// it away. The repo keeps an admin: its last one is not demoted, unless an organization holds it.
/// Who owns the space it is in stays: the person whose own repo it is, and an organization's admins,
/// are not demoted by a collaborator; a person outside its organization takes one of its seats.
pub fn set_collaborator(db: &Db, repo: &Repo, to: &str, role: Option<Role>, who: &Who, by: &str) -> Result<Value> {
    let person = person_of(db, to)?;
    let name = repo.full_name();
    if !who.root() && who.person() != Some(person.as_str()) && role < Some(Role::Admin) {
        if repo.org.is_none() && repo.space.as_deref() == Some(person.as_str()) {
            anyhow::bail!(Forbidden(format!("{name} is {person}'s own: no collaborator takes their admin role away (they move or delete the repo themselves)")));
        }
        if let (Some(o), Some(_)) = (repo.org.as_deref(), role) {
            let theirs = orgs::member(db, o, &person)?;
            let mine = orgs::member(db, o, who.person().unwrap_or_default())?;
            anyhow::ensure!(
                theirs < Some(orgs::OrgRole::Admin) || mine >= Some(orgs::OrgRole::Admin),
                Forbidden(format!("{person} is {} of {o}: only an admin of {o} changes their role on its repos", theirs.map(orgs::OrgRole::as_str).unwrap_or_default()))
            );
        }
    }
    if let (Some(o), Some(_)) = (repo.org.as_deref(), role) {
        orgs::room_for(db, o, std::slice::from_ref(&person))?;
    }
    let own = auth::roles_of_repo(db, &repo.id)?;
    let was = own.iter().find(|(p, _)| *p == person).map(|(_, r)| *r);
    if role.is_none() && was.is_none() {
        let via = repo.org.as_deref().map(|o| orgs::member(db, o, &person).map(|m| m.map(|m| (o, m)))).transpose()?.flatten();
        return Err(match via {
            Some((o, m)) => anyhow!("{person} reaches {name} as {} of {o}: an admin of {o} changes that (`avp org role {o} {person} none`), or a role here overrides it", m.as_str()),
            None => anyhow!("{person} has no role on {name}"),
        });
    }
    let admins = own.iter().filter(|(p, r)| *r == Role::Admin && *p != person).count();
    anyhow::ensure!(
        !(was == Some(Role::Admin) && role != Some(Role::Admin) && admins == 0 && repo.org.is_none()),
        "{person} is {name}'s last admin: make another person admin first (`avp repo role <person> admin`)"
    );
    auth::set_role(db, repo, &person, role, by)?;
    let said = match role {
        Some(r) => format!("{person} has {} on {name} (was {}): they {}. `avp repo show` lists who reaches it", r.as_str(), role_name(was), does(r)),
        None => format!("{person} no longer has a role on {name} (was {}): their tokens stop reaching it", role_name(was)),
    };
    Ok(json!({ "person": person, "repo": name, "role": role_name(role), "was": role_name(was), "said": said }))
}

fn does(r: Role) -> &'static str {
    match r {
        Role::Read => "read the line and clone it",
        Role::Write => "open and land tasks, and approve guarded changes",
        Role::Admin => "also change its settings, roles, tokens and guards",
    }
}

/// The tokens that reach the repo: those made for it alone, then the asker's own personal tokens if
/// they have a role on it (they reach every repo of their person). Another person's personal token
/// is theirs to see: an admin of one repo never learns its id nor its use. Only the first are
/// revoked here.
pub fn tokens(db: &Db, repo: &Repo, who: &Who) -> Result<Vec<Value>> {
    let name = repo.full_name();
    let mut out = vec![];
    let mut st = db.conn.prepare("SELECT token, role FROM repo_tokens WHERE repo = ?1")?;
    let scoped = st.query_map(params![repo.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let now = now_ms();
    let works = |t: &crate::model::TokenInfo| t.revoked_at.is_none() && t.expires_at.is_none_or(|e| e > now);
    for (id, role) in scoped {
        if let Some(t) = auth::token(db, &id)?.filter(|t| works(t)) {
            out.push(json!({ "id": t.id, "person": t.person, "name": t.name, "role": role, "scope": format!("{name}: {role}"), "created_at": t.created_at, "expires_at": t.expires_at, "used_at": t.used_at, "revocable": true }));
        }
    }
    for (person, role) in auth::roles_of_repo(db, &repo.id)?.into_iter().filter(|(p, _)| who.person() == Some(p.as_str())) {
        for t in auth::tokens(db, Some(&person))?.into_iter().filter(|t| t.kind == "token") {
            out.push(json!({ "id": t.id, "person": t.person, "name": t.name, "role": role, "scope": format!("every repo of {person}"), "created_at": t.created_at, "expires_at": t.expires_at, "used_at": t.used_at, "revocable": false }));
        }
    }
    Ok(out)
}

/// A token for an agent or a CI that reaches this repo alone, at most to `role` (read or write)
/// and never beyond its person's own role there. Shown once: the server keeps its hash.
pub fn create_token(db: &Db, repo: &Repo, who: &Who, name: &str, role: Role, expires: Option<i64>, by: &str) -> Result<Value> {
    let Some(person) = who.person().filter(|p| *p != GUEST) else {
        anyhow::bail!("a repo token belongs to a person: make it signed in as one (on a server without accounts, its one key reaches every repo)")
    };
    anyhow::ensure!(role != Role::Admin, "a repo token reads or writes (`--role read` or `write`): settings stay a person's");
    let t = auth::create_token(db, person, name, "repo", expires, by)?;
    db.conn.execute("INSERT INTO repo_tokens (token, repo, role) VALUES (?1, ?2, ?3)", params![t.info.id, repo.id, role.as_str()])?;
    db.emit(&repo.id, "repo.token.created", None, None, json!({ "id": t.info.id, "name": name, "role": role, "person": person, "expires_at": expires, "by": by }))?;
    let full = repo.full_name();
    let said = format!(
        "Shown once, never again: put it in the agent's or the CI's AVP_TOKEN. It reaches {full} alone, to {} ({}), as {person}; `avp repo token revoke {}` stops it",
        role.as_str(),
        if role == Role::Read { "read and clone" } else { "open and land tasks, never approve" },
        t.info.id
    );
    Ok(json!({ "token": t.token, "info": t.info, "repo": full, "role": role, "said": said }))
}

pub fn revoke_token(db: &Db, repo: &Repo, id: &str, by: &str) -> Result<Value> {
    let of: Option<String> = db.conn.query_row("SELECT repo FROM repo_tokens WHERE token = ?1", params![id], |r| r.get(0)).optional()?;
    let full = repo.full_name();
    anyhow::ensure!(of.as_deref() == Some(repo.id.as_str()), "no token {id} made for {full}: `avp repo show` lists them (a person's own token is theirs to revoke: `avp token revoke`)");
    let t = auth::revoke(db, id, by)?;
    db.emit(&repo.id, "repo.token.revoked", None, None, json!({ "id": id, "name": t.name, "person": t.person, "by": by }))?;
    Ok(json!({ "id": id, "name": t.name, "revoked_at": t.revoked_at, "said": format!("{id} ({}) revoked: it no longer reaches {full}", t.name) }))
}

/// Remembers a name the repo answered to, so that it keeps answering.
fn keep_name(db: &Db, space: &str, name: &str, repo: &str) -> Result<()> {
    db.conn.execute(
        "INSERT INTO repo_names (space, name, repo, at) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (space, name) DO UPDATE SET repo = excluded.repo, at = excluded.at",
        params![space, name, repo, now_ms()],
    )?;
    Ok(())
}

/// Names who created the repo in its audit: the core records the creation, not who asked for it.
pub(crate) fn created_by(db: &Db, repo: &Repo, by: &str) -> Result<()> {
    db.conn.execute("UPDATE events SET data = json_set(data, '$.by', ?2) WHERE repo = ?1 AND kind = 'repo.created'", params![repo.id, by])?;
    Ok(())
}

pub fn rename(db: &Db, repo: &Repo, new: &str, by: &str) -> Result<String> {
    let new = new.trim();
    auth::valid_name(new, "a repo's")?;
    let space = repo.space.clone().unwrap_or_default();
    let said = |n: &str| if space.is_empty() { n.to_string() } else { format!("{space}/{n}") };
    anyhow::ensure!(db.repo_in(repo.space.as_deref(), new)?.is_none(), "{} already exists: choose another name", said(new));
    keep_name(db, &space, &repo.name, &repo.id)?;
    // The new name is this repo's own now, no longer another's old one.
    db.conn.execute("DELETE FROM repo_names WHERE space = ?1 AND name = ?2", params![space, new])?;
    db.conn.execute("UPDATE repos SET name = ?2 WHERE id = ?1", params![repo.id, new])?;
    db.emit(&repo.id, "repo.renamed", None, None, json!({ "was": repo.full_name(), "name": said(new), "by": by }))?;
    Ok(format!("{} is now {}: the old address still reaches it, `git clone` included, until another repo takes the name", repo.full_name(), said(new)))
}

/// Moves the repo to another space: the asker's own, or an organization they administer (a server
/// admin: any). Its old address keeps reaching it, its roles stay, its minutes are the new space's.
pub fn transfer(db: &Db, repo: &Repo, to: &str, who: &Who) -> Result<String> {
    anyhow::ensure!(orgs::on(), "spaces are AVP Cloud's: this server's repos have none to move between");
    let to = to.trim();
    orgs::may_move_out(db, repo, who)?;
    let (org, owner): (Option<&str>, Option<String>) = if who.person() == Some(to) {
        (None, Some(to.into()))
    } else if orgs::org(db, to)?.is_some() {
        orgs::administered(db, to, who)?;
        let owner: Option<String> = db.conn.query_row("SELECT owner FROM repos WHERE id = ?1", params![repo.id], |r| r.get(0))?;
        (Some(to), owner)
    } else if who.root() && auth::user(db, to)?.is_some() {
        (None, Some(to.into()))
    } else {
        anyhow::bail!("no space {to} of yours: your own ({}) or an organization you administer (`avp org list`)", who.person().unwrap_or("none"))
    };
    anyhow::ensure!(repo.space.as_deref() != Some(to), "{} is already in {to}", repo.full_name());
    anyhow::ensure!(db.repo_in(Some(to), &repo.name)?.is_none(), "{to}/{} already exists: rename one of them first", repo.name);
    if let Some(o) = org {
        orgs::room_for(db, o, &auth::roles_of_repo(db, &repo.id)?.into_iter().map(|(p, _)| p).collect::<Vec<_>>())?;
    }
    keep_name(db, repo.space.as_deref().unwrap_or(""), &repo.name, &repo.id)?;
    db.conn.execute("DELETE FROM repo_names WHERE space = ?1 AND name = ?2", params![to, repo.name])?;
    db.conn.execute("UPDATE repos SET org = ?2, owner = ?3 WHERE id = ?1", params![repo.id, org, owner])?;
    orgs::forget_usage();
    let by = who.name();
    db.emit(&repo.id, "repo.transferred", None, None, json!({ "was": repo.full_name(), "name": format!("{to}/{}", repo.name), "by": by }))?;
    orgs::moved(db, repo.org.as_deref(), org, &repo.name, repo.space.as_deref(), to, &by)?;
    // A person's own repo is theirs to run: out of an organization, no org role gives it them.
    if let (None, Some(p)) = (org, &owner) {
        if auth::role(db, &repo.id, p)? != Some(Role::Admin) {
            auth::set_role(db, &db.repo(&repo.id)?, p, Some(Role::Admin), &by)?;
        }
    }
    Ok(format!("{} is now {to}/{}: the old address still reaches it, `git clone` included; its roles stay, its proof minutes are {to}'s", repo.full_name(), repo.name))
}

pub fn describe(db: &Db, repo: &Repo, text: &str, by: &str) -> Result<String> {
    let text = text.trim();
    anyhow::ensure!(text.chars().count() <= 300, "a description is 300 characters at most: the README says the rest");
    set(db, &repo.id, "description", text.to_string().into())?;
    db.emit(&repo.id, "repo.described", None, None, json!({ "description": text, "by": by }))?;
    Ok(if text.is_empty() { format!("{} has no description", repo.full_name()) } else { format!("{} described", repo.full_name()) })
}

pub fn set_visibility(db: &Db, repo: &Repo, visibility: &str, by: &str) -> Result<String> {
    let v = visibility.trim();
    anyhow::ensure!(v == "private" || v == "public", "{v:?} is no visibility: private (those given a role read it) or public (anyone reads it, signed in or not; nobody writes without a role)");
    anyhow::ensure!(v == "private" || public_allowed(), "this server keeps every repo private: public repos are not open on it");
    let was = settings(db, &repo.id)?.visibility;
    if was != v {
        set(db, &repo.id, "visibility", v.to_string().into())?;
        db.emit(&repo.id, "repo.visibility", None, None, json!({ "visibility": v, "was": was, "by": by }))?;
    }
    Ok(match v {
        "public" => format!("{} is public: anyone reads and clones it, signed in or not; writing still needs a role", repo.full_name()),
        _ => format!("{} is private: only those given a role reach it", repo.full_name()),
    })
}

pub fn archive(db: &Db, repo: &Repo, on: bool, by: &str) -> Result<String> {
    let s = settings(db, &repo.id)?;
    let name = repo.full_name();
    if on != s.archived_at.is_some() {
        let now = now_ms();
        set(db, &repo.id, "archived_at", if on { now.into() } else { rusqlite::types::Value::Null })?;
        set(db, &repo.id, "archived_by", if on { by.to_string().into() } else { rusqlite::types::Value::Null })?;
        db.emit(&repo.id, if on { "repo.archived" } else { "repo.unarchived" }, None, None, json!({ "by": by }))?;
    }
    Ok(if on {
        format!("{name} is archived: read-only, every task opened or landed on it is refused. `avp repo archive --off` takes it back")
    } else {
        format!("{name} is no longer archived: tasks open and land again")
    })
}

/// Deletes the repo, once the asker typed its name: it answers as missing from now on, and its data
/// goes after KEEP_DAYS, restorable until then by an admin of it.
pub fn delete(db: &Db, repo: &Repo, confirm: &str, by: &str) -> Result<String> {
    let name = repo.full_name();
    let confirm = confirm.trim();
    anyhow::ensure!(
        confirm == repo.name || confirm == name,
        "to delete {name}, type its name: `avp repo delete --confirm {}` (an agent passes it too: nothing is deleted by mistake)",
        repo.name
    );
    anyhow::ensure!(settings(db, &repo.id)?.deleted_at.is_none(), "{name} is already deleted: `avp repo restore` brings it back");
    let now = now_ms();
    set(db, &repo.id, "deleted_at", now.into())?;
    set(db, &repo.id, "deleted_by", by.to_string().into())?;
    db.emit(&repo.id, "repo.deleted", None, None, json!({ "by": by, "purge_at": purge_at(now) }))?;
    if let Some(o) = &repo.org {
        db.emit("", "org.repo.deleted", None, None, json!({ "org": o, "repo": repo.name, "by": by, "purge_at": purge_at(now) }))?;
    }
    Ok(format!(
        "{name} is deleted: it answers as missing from now on (clone, open, land). Its data goes on {} UTC; until then an admin of it restores it: `avp repo restore --repo {name}`",
        crate::blame::utc(purge_at(now))
    ))
}

pub fn restore(db: &Db, repo: &Repo, by: &str) -> Result<String> {
    let name = repo.full_name();
    anyhow::ensure!(settings(db, &repo.id)?.deleted_at.is_some(), "{name} is not deleted");
    set(db, &repo.id, "deleted_at", rusqlite::types::Value::Null)?;
    set(db, &repo.id, "deleted_by", rusqlite::types::Value::Null)?;
    db.emit(&repo.id, "repo.restored", None, None, json!({ "by": by }))?;
    if let Some(o) = &repo.org {
        db.emit("", "org.repo.restored", None, None, json!({ "org": o, "repo": repo.name, "by": by }))?;
    }
    Ok(format!("{name} is restored: as it was, its line, tasks, roles and tokens"))
}

/// The repos deleted more than KEEP_DAYS ago, gone for good: every row of every table that names
/// them (a table one adds later included), their secrets and their served git copy. Their names.
pub fn purge(e: &Engine) -> Result<Vec<String>> {
    let db = e.db.lock().unwrap();
    let mut st = db.conn.prepare("SELECT repo FROM repo_settings WHERE deleted_at IS NOT NULL AND deleted_at <= ?1")?;
    let due = st.query_map(params![now_ms() - KEEP_DAYS * DAY_MS], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut tables: Vec<(String, bool, bool)> = vec![];
    let mut names = db.conn.prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")?;
    for t in names.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()? {
        let mut cols = db.conn.prepare(&format!("PRAGMA table_info({t})"))?;
        let cols = cols.query_map([], |r| r.get::<_, String>(1))?.collect::<rusqlite::Result<Vec<_>>>()?;
        tables.push((t, cols.iter().any(|c| c == "repo"), cols.iter().any(|c| c == "task")));
    }
    let mut gone = vec![];
    for id in due {
        let name = db.repo(&id).map(|r| r.full_name()).unwrap_or_else(|_| id.clone());
        let tx = db.conn.unchecked_transaction()?;
        // A task's rows first (writes, approvals), while its task still says whose it is.
        for (t, repo, task) in &tables {
            if *task && !*repo && t != "tasks" {
                tx.execute(&format!("DELETE FROM {t} WHERE task IN (SELECT id FROM tasks WHERE repo = ?1)"), params![id])?;
            }
        }
        tx.execute("UPDATE tokens SET revoked_at = ?2 WHERE revoked_at IS NULL AND id IN (SELECT token FROM repo_tokens WHERE repo = ?1)", params![id, now_ms()])?;
        for (t, repo, _) in &tables {
            if *repo {
                tx.execute(&format!("DELETE FROM {t} WHERE repo = ?1"), params![id])?;
            }
        }
        tx.execute("DELETE FROM repos WHERE id = ?1", params![id])?;
        tx.commit()?;
        let store = super::secrets::SecretStore::new(&e.data);
        for s in store.list(&id).unwrap_or_default() {
            let _ = store.remove(&id, &s.name);
        }
        let _ = std::fs::remove_dir_all(crate::gitbridge::served_root(&e.data).join(format!("{id}.git")));
        gone.push(name);
    }
    Ok(gone)
}

/// What guards the repo, read only: its guarded paths (the server's), the files always guarded, its
/// landing rule, and who may approve (a person with write or admin; a repo token never does).
pub fn security(db: &Db, repo: &Repo) -> Result<Value> {
    let approvers: Vec<String> = collaborators(db, repo)?.into_iter().filter(|c| c["role"] != "read").filter_map(|c| c["person"].as_str().map(String::from)).collect();
    Ok(json!({
        "guarded": db.guarded(&repo.id)?,
        "always_guarded": CHECK_FILES,
        "landing": repo.landing,
        "approvers": approvers,
        "said": "A task that changes a guarded path waits for a person's approval before it lands (`avp approve <task>`): a person with write or admin gives it, never a repo token. Admins change the guards with `avp guard`.",
    }))
}

/// The repo's audit log, newest first: who changed a setting, a role, a token, a guard, a secret, when.
pub fn audit(db: &Db, repo: &str, before: Option<i64>, limit: i64) -> Result<Vec<Value>> {
    let mut st = db.conn.prepare(
        "SELECT id, at, kind, data FROM events WHERE repo = ?1 AND (?2 IS NULL OR id < ?2) \
         AND (kind LIKE 'repo.%' OR kind IN ('role.set', 'secret.set', 'secret.removed')) ORDER BY id DESC LIMIT ?3",
    )?;
    let rows = st.query_map(params![repo, before, limit.clamp(1, 1000)], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))?;
    let mut out = vec![];
    for row in rows {
        let (id, at, kind, data) = row?;
        let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
        let mut by = data["by"].as_str().or(data["person"].as_str().filter(|_| kind.starts_with("secret."))).map(String::from);
        // A repo's creation names nobody: its maker is who gave the first role, theirs.
        if by.is_none() && kind == "repo.created" {
            by = db.conn.query_row("SELECT json_extract(data, '$.by') FROM events WHERE repo = ?1 AND kind = 'role.set' ORDER BY id LIMIT 1", params![repo], |r| r.get(0)).optional()?.flatten();
        }
        out.push(json!({ "id": id, "at": at, "kind": kind, "by": by, "what": what(&kind, &data), "data": data }));
    }
    Ok(out)
}

/// An event of the log, said in a few words.
pub fn what(kind: &str, d: &Value) -> String {
    let s = |k: &str| d[k].as_str().unwrap_or_default().to_string();
    match kind {
        "repo.created" => "created the repo".into(),
        "role.set" if d["role"] == "none" => format!("took {}'s role away (was {})", s("person"), s("was")),
        "role.set" => format!("gave {} {} (was {})", s("person"), s("role"), s("was")),
        "repo.token.created" => format!("made the token {} ({}, {})", s("name"), s("role"), s("id")),
        "repo.token.revoked" => format!("revoked the token {} ({})", s("name"), s("id")),
        "repo.renamed" | "repo.transferred" => format!("{} {} to {}", if kind == "repo.renamed" { "renamed" } else { "moved" }, s("was"), s("name")),
        "repo.described" => format!("described it: {:?}", s("description")),
        "repo.visibility" => format!("made it {} (was {})", s("visibility"), s("was")),
        "repo.archived" => "archived it".into(),
        "repo.unarchived" => "unarchived it".into(),
        "repo.deleted" => "deleted it".into(),
        "repo.restored" => "restored it".into(),
        "repo.guarded" => format!("guarded paths now {}", d["guarded"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty()).unwrap_or_else(|| "none".into())),
        "repo.landing" => format!("landing rule now {}", s("landing")),
        "secret.set" => format!("set the secret {}", s("name")),
        "secret.removed" => format!("removed the secret {}", s("name")),
        other => other.to_string(),
    }
}

/// The spaces the asker may move a repo to: their own, and the organizations they administer.
fn spaces(db: &Db, who: &Who) -> Result<Vec<String>> {
    let Some(me) = who.person() else { return Ok(vec![]) };
    let mut out = vec![me.to_string()];
    for (o, r) in orgs::orgs_of(db, me)? {
        if r >= orgs::OrgRole::Admin {
            out.push(o.name);
        }
    }
    Ok(out)
}

/// Everything the settings page shows, in one answer.
pub fn show(db: &Db, repo: &Repo, who: &Who) -> Result<Value> {
    let s = settings(db, &repo.id)?;
    let mut st = db.conn.prepare("SELECT space, name FROM repo_names WHERE repo = ?1 ORDER BY at")?;
    let before: Vec<String> = st.query_map(params![repo.id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.filter_map(|r| r.ok()).map(|(s, n)| if s.is_empty() { n } else { format!("{s}/{n}") }).collect();
    let name = repo.full_name();
    let state = match (s.deleted_at, s.archived_at) {
        (Some(at), _) => format!("deleted: its data goes on {} UTC unless an admin restores it (`avp repo restore`)", crate::blame::utc(purge_at(at))),
        (None, Some(_)) => "archived: read-only (`avp repo archive --off` takes it back)".into(),
        _ => format!("{}: {}", s.visibility, if s.visibility == "public" { "anyone reads it" } else { "those given a role reach it" }),
    };
    Ok(json!({
        "repo": repo,
        "name": name,
        "description": s.description,
        "visibility": s.visibility,
        "public_allowed": public_allowed(),
        "archived": s.archived_at.map(|at| json!({ "at": at, "by": s.archived_by })),
        "deleted": s.deleted_at.map(|at| json!({ "at": at, "by": s.deleted_by, "purge_at": purge_at(at) })),
        "names_before": before,
        "collaborators": collaborators(db, repo)?,
        "org": repo.org,
        "tokens": tokens(db, repo, who)?,
        "security": security(db, repo)?,
        "spaces": if orgs::on() { spaces(db, who)? } else { vec![] },
        "cloud": orgs::on(),
        "said": format!("{name} is {state}. Change it with `avp repo invite|role|token|rename|describe|visibility|transfer|archive|delete`"),
    }))
}

// ---- the routes ----

type S = State<Engine>;
use super::auth::run;

pub fn routes() -> Router<Engine> {
    Router::new()
        .route("/repos/{repo}/settings", get(show_route))
        .route("/repos/{repo}/settings/collaborators", post(collaborator_route))
        .route("/repos/{repo}/settings/tokens", post(token_route))
        .route("/repos/{repo}/settings/tokens/{id}/revoke", post(revoke_route))
        .route("/repos/{repo}/settings/general", post(general_route))
        .route("/repos/{repo}/settings/transfer", post(transfer_route))
        .route("/repos/{repo}/settings/archive", post(archive_route))
        .route("/repos/{repo}/settings/delete", post(delete_route))
        .route("/repos/{repo}/settings/restore", post(restore_route))
        .route("/repos/{repo}/settings/audit", get(audit_route))
        .route("/repos/{repo}/about", get(about_route))
}

/// What its readers see of a repo's settings: its description, its visibility, whether it is
/// archived (the website hides what an archived repo refuses).
async fn about_route(State(e): S, P(repo): P<String>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = db.repo(&repo)?;
        let s = settings(&db, &r.id)?;
        Ok(json!({ "name": r.full_name(), "description": s.description, "visibility": s.visibility, "archived": s.archived_at.map(|at| json!({ "at": at, "by": s.archived_by })) }))
    })
    .await
}

async fn show_route(State(e): S, Extension(who): Extension<Who>, P(repo): P<String>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        show(&db, &r, &who)
    })
    .await
}

#[derive(Deserialize)]
struct Collaborator {
    /// A person's name, or their email.
    person: String,
    role: String,
}

async fn collaborator_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<Collaborator>) -> Response {
    run(move || {
        let role = Role::parse(&b.role)?;
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        set_collaborator(&db, &r, &b.person, role, &who, &who.by(&headers))
    })
    .await
}

#[derive(Deserialize)]
struct NewToken {
    name: Option<String>,
    role: Option<String>,
    expires: Option<String>,
}

async fn token_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<NewToken>) -> Response {
    run(move || {
        let role = Role::parse(b.role.as_deref().unwrap_or("read"))?.ok_or_else(|| anyhow!("a repo token reads or writes: `--role read` or `write`"))?;
        let expires = auth::expiry(b.expires.as_deref().unwrap_or("never"), now_ms())?;
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        let name = b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).unwrap_or("agent");
        create_token(&db, &r, &who, name, role, expires, &who.by(&headers))
    })
    .await
}

async fn revoke_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P((repo, id)): P<(String, String)>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        revoke_token(&db, &r, &id, &who.by(&headers))
    })
    .await
}

#[derive(Deserialize)]
struct General {
    name: Option<String>,
    description: Option<String>,
    visibility: Option<String>,
}

async fn general_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<General>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        let by = who.by(&headers);
        let mut said = vec![];
        if let Some(d) = &b.description {
            said.push(describe(&db, &r, d, &by)?);
        }
        if let Some(v) = &b.visibility {
            said.push(set_visibility(&db, &r, v, &by)?);
        }
        if let Some(n) = b.name.as_deref().filter(|n| n.trim() != r.name) {
            said.push(rename(&db, &r, n, &by)?);
        }
        anyhow::ensure!(!said.is_empty(), "nothing to change: `name`, `description` or `visibility`");
        let now = db.repo(&r.id)?;
        Ok(json!({ "repo": now, "name": now.full_name(), "said": said.join(". ") }))
    })
    .await
}

#[derive(Deserialize)]
struct Transfer {
    to: String,
}

async fn transfer_route(State(e): S, Extension(who): Extension<Who>, P(repo): P<String>, Json(b): Json<Transfer>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        let said = transfer(&db, &r, &b.to, &who)?;
        let now = db.repo(&r.id)?;
        Ok(json!({ "repo": now, "name": now.full_name(), "said": said }))
    })
    .await
}

#[derive(Deserialize)]
struct Archive {
    archived: bool,
}

async fn archive_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<Archive>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        Ok(json!({ "name": r.full_name(), "archived": b.archived, "said": archive(&db, &r, b.archived, &who.by(&headers))? }))
    })
    .await
}

#[derive(Deserialize)]
struct Delete {
    #[serde(default)]
    confirm: String,
}

async fn delete_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>, Json(b): Json<Delete>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        Ok(json!({ "name": r.full_name(), "deleted": true, "said": delete(&db, &r, &b.confirm, &who.by(&headers))? }))
    })
    .await
}

async fn restore_route(State(e): S, Extension(who): Extension<Who>, headers: HeaderMap, P(repo): P<String>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        Ok(json!({ "name": r.full_name(), "deleted": false, "said": restore(&db, &r, &who.by(&headers))? }))
    })
    .await
}

#[derive(Deserialize)]
struct AuditQuery {
    before: Option<i64>,
    limit: Option<i64>,
}

async fn audit_route(State(e): S, Extension(who): Extension<Who>, P(repo): P<String>, Query(q): Query<AuditQuery>) -> Response {
    run(move || {
        let db = e.db.lock().unwrap();
        let r = administered(&db, &repo, &who)?;
        audit(&db, &r.id, q.before, q.limit.unwrap_or(100))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (Db, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("avp-test-settings-{}", ulid::Ulid::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("avp.db")).unwrap();
        tables(&db).unwrap();
        (db, dir)
    }

    /// A renamed repo's old name keeps reaching it, through the API and `git clone`, until another
    /// repo takes the name.
    #[test]
    fn an_old_name_reaches_the_renamed_repo() {
        let (db, _dir) = db();
        let r = db.create_repo("shop", crate::model::LeasePolicy::Advisory).unwrap();
        rename(&db, &r, "store", "ada").unwrap();
        assert_eq!(aliased(&db, "/repos/shop/line").unwrap(), Some(format!("/repos/{}/line", r.id)));
        assert_eq!(aliased(&db, "/shop.git/info/refs").unwrap(), Some(format!("/{}.git/info/refs", r.id)));
        assert_eq!(aliased(&db, "/repos/store/line").unwrap(), None);
        db.create_repo("shop", crate::model::LeasePolicy::Advisory).unwrap();
        assert_eq!(aliased(&db, "/repos/shop/line").unwrap(), None, "a new repo of that name is that name's");
    }

    /// An admin of a repo sees the tokens made for it and their own, never a collaborator's
    /// personal token (its id, its name, its last use).
    #[test]
    fn an_admin_never_sees_a_collaborators_personal_token() {
        let (db, _dir) = db();
        let r = db.create_repo("shop", crate::model::LeasePolicy::Advisory).unwrap();
        let (ada, ada_first) = auth::add_user(&db, "ada", None, None, false, "server").unwrap();
        let (bob, bob_first) = auth::add_user(&db, "bob", None, None, false, "ada").unwrap();
        auth::set_role(&db, &r, "ada", Some(Role::Admin), "ada").unwrap();
        auth::set_role(&db, &r, "bob", Some(Role::Write), "ada").unwrap();
        let ci = create_token(&db, &r, &Who::Person(bob), "ci", Role::Read, None, "bob").unwrap();
        let ids: Vec<String> = tokens(&db, &r, &Who::Person(ada)).unwrap().iter().map(|t| t["id"].as_str().unwrap().to_string()).collect();
        assert!(ids.contains(&ada_first.info.id), "their own: {ids:?}");
        assert!(ids.iter().any(|i| ci["info"]["id"] == i.as_str()), "the repo's: {ids:?}");
        assert!(!ids.contains(&bob_first.info.id), "bob's own token is bob's: {ids:?}");
    }

    /// Archived: reads go on, nothing lands. Deleted: missing, but to its settings' restore.
    #[test]
    fn an_archived_repo_refuses_a_landing_and_a_deleted_one_answers_as_missing() {
        let (db, _dir) = db();
        let r = db.create_repo("shop", crate::model::LeasePolicy::Advisory).unwrap();
        archive(&db, &r, true, "ada").unwrap();
        let (code, said) = closed(&db, &Method::POST, "/repos/shop/tasks/t1/land").unwrap().unwrap();
        assert_eq!(code, StatusCode::FORBIDDEN);
        assert!(said.contains("archived") && said.contains("--off"), "{said}");
        assert!(closed(&db, &Method::GET, "/repos/shop/line").unwrap().is_none());
        assert!(closed(&db, &Method::POST, "/shop.git/git-receive-pack").unwrap().is_some());
        assert!(delete(&db, &r, "other", "ada").is_err(), "the name typed is the repo's");
        delete(&db, &r, "shop", "ada").unwrap();
        assert_eq!(closed(&db, &Method::GET, "/shop.git/info/refs").unwrap().unwrap().0, StatusCode::NOT_FOUND);
        assert!(closed(&db, &Method::POST, "/repos/shop/settings/restore").unwrap().is_none());
        assert!(live(&db, db.repos().unwrap()).unwrap().is_empty());
    }
}
