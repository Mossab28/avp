//! The CLI's side of the HTTP surface. Blocking on purpose: a CLI call is one request.

use crate::model::*;
use anyhow::{anyhow, Result};
use reqwest::blocking::{Client as Http, Response};
use serde::de::DeserializeOwned;

/// The credential sent with every request when the server's address carries none: `AVP_TOKEN`, as
/// Basic auth with the person's name (`AVP_USER`, else `AVP_PERSON`), which both a proxy's per-person
/// passwords and the server's own `AVP_TOKEN` check accept. `AVP_SERVER` stays a plain address, so
/// the secret never sits in a URL that ends up in configs, process lists and logs.
fn token_header(base: &str) -> Option<reqwest::header::HeaderValue> {
    use base64::Engine as _;
    let has_userinfo = base.split_once("://").is_some_and(|(_, rest)| rest.split('/').next().is_some_and(|host| host.contains('@')));
    if has_userinfo {
        return None;
    }
    let (user, token) = match std::env::var("AVP_TOKEN").ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
        Some(token) => (["AVP_USER", "AVP_PERSON"].iter().find_map(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty())).unwrap_or_else(|| "avp".into()), token),
        // No token in the environment: the one `avp login` kept for this server, if any.
        None => Credentials::load().get(base).map(|l| (l.person.clone(), l.token.clone()))?,
    };
    let mut value = reqwest::header::HeaderValue::from_str(&format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(format!("{}:{token}", user.trim())))).ok()?;
    value.set_sensitive(true);
    Some(value)
}

/// AVP Cloud, what `avp login` logs in to unless told another server: `AVP_CLOUD_URL` when this
/// avp runs, else as it was built, else AVP's own.
pub fn cloud_url() -> String {
    let set = |v: Option<String>| v.map(|v| v.trim().trim_end_matches('/').to_string()).filter(|v| !v.is_empty());
    set(std::env::var("AVP_CLOUD_URL").ok()).or_else(|| set(option_env!("AVP_CLOUD_URL").map(String::from))).unwrap_or_else(|| "https://cloud.murmell.com".into())
}

/// A server's address as credentials know it: `https://` when none is said, no trailing slash.
pub fn server_url(s: &str) -> String {
    let s = s.trim().trim_end_matches('/');
    if s.contains("://") { s.to_string() } else { format!("https://{s}") }
}

/// One server's login, kept by `avp login`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Login {
    pub person: String,
    pub token: String,
    /// The token's id, for `avp logout` to revoke it (none for a token given with `--token`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_id: Option<String>,
    pub at: i64,
}

/// What `avp login` keeps, in ~/.config/avp/credentials (readable by its owner alone): a login per
/// server, and the one logged in to last, which every command talks to when nothing names another
/// (no AVP_SERVER, no --server, no view of another server).
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(default)]
    pub servers: std::collections::BTreeMap<String, Login>,
}

impl Credentials {
    /// `$XDG_CONFIG_HOME/avp/credentials`, else `~/.config/avp/credentials`.
    pub fn path() -> std::path::PathBuf {
        let config = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()).map(std::path::PathBuf::from);
        config.unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into())).join(".config")).join("avp").join("credentials")
    }

    /// The file's logins; none when it is missing or unreadable (a command then asks as before).
    pub fn load() -> Self {
        Self::read(&Self::path())
    }

    fn read(p: &std::path::Path) -> Self {
        std::fs::read_to_string(p).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
    }

    /// Written whole, readable by its owner alone, its folder too: a token is a password.
    pub fn save(&self) -> Result<()> {
        self.write(&Self::path())
    }

    fn write(&self, p: &std::path::Path) -> Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
        let dir = p.parent().ok_or_else(|| anyhow!("{}: no folder", p.display()))?;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
        let text = format!("# avp login: a token per server, a password each. `avp logout` takes one out.\n{}", toml::to_string(self)?);
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(p)?;
        // A file made before with wider permissions keeps them through `mode`: narrowed first.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(text.as_bytes())?;
        Ok(())
    }

    /// The login kept for the server at `base` (its address alone: a path after it is the repo's).
    pub fn get(&self, base: &str) -> Option<&Login> {
        self.servers.get(&server_url(base))
    }

    pub fn set(&mut self, server: &str, login: Login) {
        let server = server_url(server);
        self.current = Some(server.clone());
        self.servers.insert(server, login);
    }

    /// Takes the server's login out; the last one logged in to stays current, if any is left.
    pub fn remove(&mut self, server: &str) -> Option<Login> {
        let server = server_url(server);
        let gone = self.servers.remove(&server);
        if self.current.as_deref() == Some(server.as_str()) {
            self.current = self.servers.iter().max_by_key(|(_, l)| l.at).map(|(s, _)| s.clone());
        }
        gone
    }
}

/// The server logged in to last, for a command that names none.
pub fn logged_in() -> Option<String> {
    let c = Credentials::load();
    c.current.filter(|s| c.servers.contains_key(s))
}

/// The server did not answer: a proxy in front of it says it is down (502, 503, 504), or AVP's
/// offline mode knows it already. Like a connection refused, it sends a command to the outbox.
#[derive(Debug)]
pub struct Unreachable(pub String);

impl std::fmt::Display for Unreachable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unreachable {}

/// Whether `e` says the server did not answer (no connection, a timeout, an answer cut short, a
/// proxy that cannot reach it), rather than answered no.
pub fn unreachable(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<Unreachable>() || c.downcast_ref::<reqwest::Error>().is_some_and(|r| !r.is_builder() && !r.is_status()))
}

/// What a server built without the cloud feature answers on one of its routes (`<what> is left out
/// of this avp`), said with what to do: run a build that carries it.
#[derive(Debug)]
pub struct CloudOnly(pub String);

impl std::fmt::Display for CloudOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\n  the release binaries (install.sh) and a default `cargo build` carry it: run one of them as the server", self.0)
    }
}

impl std::error::Error for CloudOnly {}

/// How a server from before every build carried the cloud said the same.
const CLOUD_BEFORE: &str = "part of AVP Cloud";

/// Whether `e` is a server built without the verb's feature: asking again will not change it.
pub fn cloud_only(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.is::<CloudOnly>())
}

/// An answer's status as an error: one a proxy gives for a server it cannot reach is `Unreachable`.
fn refused(status: impl std::fmt::Display + Into<u16>, msg: String) -> anyhow::Error {
    let shown = status.to_string();
    match status.into() {
        502..=504 => Unreachable(format!("{shown}: {msg}")).into(),
        404 if msg.ends_with(crate::seam::CLOUD) || msg.ends_with(CLOUD_BEFORE) => CloudOnly(msg).into(),
        _ => anyhow!("{shown}: {msg}"),
    }
}

#[derive(Clone)]
pub struct Client {
    pub base: String,
    http: Http,
}

impl Client {
    pub fn new(base: &str) -> Self {
        Self::build(base, token_header(base))
    }

    /// A client that presents `token` whatever the environment and the credentials say: what
    /// `avp login --token` checks, what `avp logout` revokes.
    pub fn with_token(base: &str, token: &str) -> Self {
        let auth = reqwest::header::HeaderValue::from_str(&format!("Bearer {}", token.trim())).ok().map(|mut v| {
            v.set_sensitive(true);
            v
        });
        Self::build(base, auth)
    }

    fn build(base: &str, auth: Option<reqwest::header::HeaderValue>) -> Self {
        // A connection that does not come within seconds will not come: the Wi-Fi dropped, and
        // the command goes to the outbox instead of hanging. Once connected, nothing cuts a request
        // (the blocking client's default is 30 s): a land waits for its proof, a download for its
        // last byte, however long; TCP keepalive (reqwest's, every 15 s) finds a link gone dead.
        let mut http = Http::builder().timeout(None).connect_timeout(std::time::Duration::from_secs(5));
        if let Some(auth) = auth {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(reqwest::header::AUTHORIZATION, auth);
            http = http.default_headers(headers);
        }
        Self { base: base.trim_end_matches('/').to_string(), http: http.build().unwrap() }
    }

    /// The server's address as it may be printed or logged: `https://user@host`, never the password.
    pub fn shown(&self) -> String {
        match self.base.split_once("://") {
            Some((scheme, rest)) => match rest.split_once('@') {
                Some((auth, host)) => format!("{scheme}://{}@{host}", auth.split(':').next().unwrap_or("")),
                None => self.base.clone(),
            },
            None => self.base.clone(),
        }
    }

    /// The server's site, for a page a person opens: its address without the credentials it may carry.
    pub fn site(&self) -> String {
        match self.base.split_once("://") {
            Some((scheme, rest)) => format!("{scheme}://{}", rest.rsplit_once('@').map(|(_, host)| host).unwrap_or(rest)),
            None => self.base.clone(),
        }
    }

    fn take<T: DeserializeOwned>(r: Response) -> Result<T> {
        let status = r.status();
        let text = r.text()?;
        if !status.is_success() {
            let msg = serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from)).unwrap_or(text.clone());
            return Err(refused(status, msg));
        }
        Ok(serde_json::from_str(&text).map_err(|e| anyhow!("bad answer: {e}: {text}"))?)
    }

    /// Whether the server answers now, asked once, for at most 5 seconds: any answer but a proxy's
    /// saying it cannot reach it (a refused token is the server answering).
    pub fn reachable(&self) -> bool {
        self.http.get(format!("{}/", self.base)).timeout(std::time::Duration::from_secs(5)).send().is_ok_and(|r| !matches!(r.status().as_u16(), 502..=504))
    }

    pub fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::take(self.http.get(format!("{}{path}", self.base)).send()?)
    }
    pub fn post<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<T> {
        Self::take(self.http.post(format!("{}{path}", self.base)).json(body).send()?)
    }

    /// The positions that changed `path` from `from` (none: the head) back, newest first, `limit` of
    /// them at most: the server answers a page per `blame::WALK` positions read, and the next page
    /// is asked until there are enough.
    pub fn file_log(&self, repo: &str, path: &str, from: Option<i64>, limit: usize) -> Result<crate::blame::FileLog> {
        let ask = |from: Option<i64>, limit: usize| -> Result<crate::blame::FileLog> {
            let mut q = vec![("path", path.to_string()), ("limit", limit.to_string())];
            q.extend(from.map(|f| ("from", f.to_string())));
            Self::take(self.http.get(format!("{}/repos/{repo}/history/log", self.base)).query(&q).send()?)
        };
        let mut log = ask(from, limit)?;
        while let Some(from) = log.more.filter(|_| log.changes.len() < limit) {
            let page = ask(Some(from), limit - log.changes.len())?;
            log.changes.extend(page.changes);
            log.more = page.more;
        }
        Ok(log)
    }

    /// Each line of `path` at position `at` (none: the head), with the position that last changed it.
    pub fn blame(&self, repo: &str, path: &str, at: Option<i64>) -> Result<crate::blame::Blame> {
        let mut q = vec![("path", path.to_string())];
        q.extend(at.map(|a| ("at", a.to_string())));
        Self::take(self.http.get(format!("{}/repos/{repo}/history/blame", self.base)).query(&q).send()?)
    }

    pub fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        Self::take(self.http.delete(format!("{}{path}", self.base)).send()?)
    }

    /// A POST whose answer may be "nothing" (204, a long poll that found no work).
    pub fn post_maybe<T: DeserializeOwned>(&self, path: &str, body: &impl serde::Serialize) -> Result<Option<T>> {
        let r = self.http.post(format!("{}{path}", self.base)).json(body).send()?;
        if r.status().as_u16() == 204 {
            return Ok(None);
        }
        Ok(Some(Self::take(r)?))
    }

    pub fn get_bytes(&self, path: &str) -> Result<Vec<u8>> {
        let r = self.http.get(format!("{}{path}", self.base)).send()?;
        anyhow::ensure!(r.status().is_success(), "{path}: {}", r.status());
        Ok(r.bytes()?.to_vec())
    }

    pub fn has_object(&self, repo: &str, hash: &str) -> Result<bool> {
        Ok(self.http.head(format!("{}/repos/{repo}/objects/{hash}", self.base)).send()?.status().is_success())
    }
    pub fn put_object(&self, repo: &str, bytes: Vec<u8>) -> Result<String> {
        let hash = crate::objects::hash_bytes(&bytes);
        if self.has_object(repo, &hash)? {
            return Ok(hash);
        }
        let r = self.http.put(format!("{}/repos/{repo}/objects/{hash}", self.base)).body(bytes).send()?;
        let _: serde_json::Value = Self::take(r)?;
        Ok(hash)
    }
    /// A file as the body of a PUT, streamed from disk: a git history may not fit in memory.
    pub fn put_file<T: DeserializeOwned>(&self, path: &str, file: &std::path::Path) -> Result<T> {
        let f = std::fs::File::open(file)?;
        let len = f.metadata()?.len();
        Self::take(self.http.put(format!("{}{path}", self.base)).header(reqwest::header::CONTENT_LENGTH, len).body(reqwest::blocking::Body::sized(f, len)).send()?)
    }
    pub fn get_object(&self, repo: &str, hash: &str) -> Result<Vec<u8>> {
        let r = self.http.get(format!("{}/repos/{repo}/objects/{hash}", self.base)).send()?;
        if !r.status().is_success() {
            return Err(refused(r.status(), format!("object {hash}")));
        }
        Ok(r.bytes()?.to_vec())
    }

    /// Objects in the order asked, in one answer, each checked against its hash. A server from
    /// before batches answers 404 or 405: then one request each.
    pub fn get_objects(&self, repo: &str, hashes: &[String]) -> Result<Vec<Vec<u8>>> {
        let r = self.http.post(format!("{}/repos/{repo}/objects", self.base)).json(&serde_json::json!({ "hashes": hashes })).send()?;
        if matches!(r.status().as_u16(), 404 | 405) {
            return hashes.iter().map(|h| self.get_object(repo, h)).collect();
        }
        anyhow::ensure!(r.status().is_success(), "objects: {}", r.status());
        let body = r.bytes()?;
        let mut out = Vec::with_capacity(hashes.len());
        let mut at = 0;
        for h in hashes {
            let nl = at + body[at..].iter().position(|b| *b == b'\n').ok_or_else(|| anyhow!("objects: the answer stops before {h}"))?;
            let head = std::str::from_utf8(&body[at..nl])?;
            let (got, len) = head.split_once(' ').ok_or_else(|| anyhow!("objects: bad header {head:?}"))?;
            anyhow::ensure!(got == h, "objects: {got} where {h} was asked");
            let end = nl + 1 + len.parse::<usize>()?;
            let bytes = body.get(nl + 1..end).ok_or_else(|| anyhow!("objects: {h} cut short"))?.to_vec();
            anyhow::ensure!(crate::objects::hash_bytes(&bytes) == *h, "object {h}: its content does not match its hash");
            out.push(bytes);
            at = end;
        }
        Ok(out)
    }

    /// A POST whose answer may take minutes (a proof, a deploy): the server keeps the connection
    /// alive with spaces, then wraps its answer as `{"status", "body"}`. A server from before that
    /// answers plainly. Gives the status and the body either way.
    fn post_long(&self, path: &str, body: Option<&serde_json::Value>) -> Result<(u16, String)> {
        let mut req = self.http.post(format!("{}{path}", self.base)).header("x-avp-keepalive", "1");
        if let Some(b) = body {
            req = req.json(b);
        }
        let r = req.send()?;
        let status = r.status().as_u16();
        let text = r.text()?;
        if let Ok(serde_json::Value::Object(o)) = serde_json::from_str::<serde_json::Value>(text.trim_start()) {
            if let (2, Some(s), Some(b)) = (o.len(), o.get("status").and_then(|s| s.as_u64()), o.get("body")) {
                return Ok((s as u16, b.to_string()));
            }
        }
        Ok((status, text))
    }

    /// Lands: Ok(Landed) or Err carrying the NotLanded answer.
    pub fn land(&self, repo: &str, task: &str) -> Result<Result<Landed, NotLanded>> {
        self.land_as(repo, task, false)
    }

    /// `stack`: the open tasks it is stacked on land first, bottom first (`avp land --stack`).
    pub fn land_as(&self, repo: &str, task: &str, stack: bool) -> Result<Result<Landed, NotLanded>> {
        let stack = if stack { "?stack=true" } else { "" };
        self.land_at(&format!("/repos/{repo}/tasks/{task}/land{stack}"))
    }

    /// `with`: open tasks landed with it as one position, proven whole (`avp land --with`).
    pub fn land_with(&self, repo: &str, task: &str, with: &[String]) -> Result<Result<Landed, NotLanded>> {
        self.land_at(&format!("/repos/{repo}/tasks/{task}/land?with={}", with.join(",")))
    }

    fn land_at(&self, path: &str) -> Result<Result<Landed, NotLanded>> {
        match self.post_long(path, None)? {
            (409, text) => Ok(Err(serde_json::from_str(&text)?)),
            (s, text) if (200..300).contains(&s) => Ok(Ok(serde_json::from_str(&text)?)),
            (s, text) => Err(refused(s, serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text))),
        }
    }

    /// The task's proof, run now if its tree has none.
    pub fn check(&self, repo: &str, task: &str) -> Result<Proof> {
        match self.post_long(&format!("/repos/{repo}/tasks/{task}/check"), None)? {
            (s, text) if (200..300).contains(&s) => Ok(serde_json::from_str(&text)?),
            (s, text) => Err(refused(s, serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text))),
        }
    }

    /// Promotes: the answer comes once a deploy AVP runs is over, however long it takes.
    pub fn promote(&self, repo: &str, env: &str, body: &serde_json::Value) -> Result<Environment> {
        match self.post_long(&format!("/repos/{repo}/envs/{env}"), Some(body))? {
            (s, text) if (200..300).contains(&s) => Ok(serde_json::from_str(&text)?),
            (s, text) => Err(refused(s, serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|v| v["error"].as_str().map(String::from)).unwrap_or(text))),
        }
    }
}

/// Lines added and removed in a file's diff.
pub fn diff_counts(f: &FileDiff) -> (usize, usize) {
    f.patch.lines().fold((0, 0), |(a, d), l| match l.as_bytes().first() {
        Some(b'+') => (a + 1, d),
        Some(b'-') => (a, d + 1),
        _ => (a, d),
    })
}

fn paint(color: bool, code: &str, s: &str) -> String {
    if color && !s.is_empty() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// `avp diff` as git prints a diff, so that `git apply` and `patch -p1` take it; colored for a terminal.
pub fn diff_text(files: &[crate::engine::Diffed], color: bool) -> String {
    let mut out = String::new();
    for crate::engine::Diffed { file: f, large } in files {
        let (a, b) = (format!("a/{}", f.path), format!("b/{}", f.path));
        let (a, b) = match f.change.as_str() {
            "added" => ("/dev/null".to_string(), b),
            "deleted" => (a, "/dev/null".to_string()),
            _ => (a, b),
        };
        if f.binary {
            out.push_str(&format!("Binary files {a} and {b} differ\n"));
            continue;
        }
        out.push_str(&paint(color, "1", &format!("--- {a}\n+++ {b}")));
        out.push('\n');
        if *large {
            out.push_str(&format!("{} is too large to show here: read the file\n", f.path));
        }
        for l in f.patch.lines() {
            let code = match l.as_bytes().first() {
                Some(b'+') => "32",
                Some(b'-') => "31",
                Some(b'@') => "36",
                _ => "",
            };
            out.push_str(&if code.is_empty() { l.to_string() } else { paint(color, code, l) });
            out.push('\n');
        }
    }
    out
}

/// `avp diff --stat`: each file with the lines it adds and removes, then the totals, as git prints them.
pub fn diff_stat(files: &[crate::engine::Diffed], color: bool) -> String {
    let files: Vec<&FileDiff> = files.iter().map(|d| &d.file).collect();
    let counts: Vec<(usize, usize)> = files.iter().map(|f| diff_counts(f)).collect();
    let width = files.iter().map(|f| f.path.chars().count()).max().unwrap_or(0);
    let most = counts.iter().map(|(a, d)| a + d).max().unwrap_or(0).max(1);
    let digits = most.to_string().len();
    // Bars of 40 at most: a file of thousands of lines still fits a terminal.
    let bar = |n: usize| if most <= 40 { n } else { (n * 40).div_ceil(most) };
    let mut out = String::new();
    for (f, (a, d)) in files.iter().zip(&counts) {
        let changes = if f.binary { "Bin".to_string() } else { format!("{:>digits$} {}{}", a + d, paint(color, "32", &"+".repeat(bar(*a))), paint(color, "31", &"-".repeat(bar(*d)))) };
        out.push_str(&format!(" {:<width$} | {changes}\n", f.path));
    }
    let (added, removed) = counts.iter().fold((0, 0), |(x, y), (a, d)| (x + a, y + d));
    let s = |n: usize| if n == 1 { "" } else { "s" };
    out.push_str(&format!(" {} file{} changed, {added} insertion{}(+), {removed} deletion{}(-)\n", files.len(), s(files.len()), s(added), s(removed)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn credentials_keep_a_login_per_server_for_their_owner_alone() {
        let dir = std::env::temp_dir().join(format!("avp-creds-{}", ulid::Ulid::new()));
        let p = dir.join("avp").join("credentials");
        let login = |person: &str, at: i64| Login { person: person.into(), token: format!("avp_{person}"), token_id: Some(format!("tok_{person}")), at };
        let mut c = Credentials::default();
        c.set("cloud.example.com/", login("ada", 1));
        c.set("http://127.0.0.1:7117", login("bob", 2));
        c.write(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600, "a token is a password");
        assert_eq!(std::fs::metadata(p.parent().unwrap()).unwrap().permissions().mode() & 0o777, 0o700);
        let back = Credentials::read(&p);
        assert_eq!(back, c);
        assert_eq!(back.current.as_deref(), Some("http://127.0.0.1:7117"), "the last login is the one commands talk to");
        assert_eq!(back.get("https://cloud.example.com").map(|l| l.person.as_str()), Some("ada"), "one server, however its address is written");
        assert!(back.get("https://other.example.com").is_none(), "a token goes to its own server alone");

        // Wider permissions from before are narrowed on the next write.
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut c = back;
        assert_eq!(c.remove("http://127.0.0.1:7117").map(|l| l.person), Some("bob".into()));
        assert_eq!(c.current.as_deref(), Some("https://cloud.example.com"), "logged out of one, the other stays current");
        c.write(&p).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        assert!(Credentials::read(&dir.join("none")).servers.is_empty(), "no file: no login");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
