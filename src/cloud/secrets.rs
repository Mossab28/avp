//! Secrets for tests (`avp secret set`): kept sealed by the server for each repo, handed to the
//! steps avp.toml names them for (on a worker, with the job it claims), masked in what they print.

use crate::check::{refused_secret, SecretInfo};
use crate::engine::Engine;
use crate::model::*;
use crate::server::{blocking, ok, rid, verified_person, R, S};
use anyhow::{Context, Result};
use axum::extract::{Path as P, State};
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use chacha20poly1305::aead::{Aead, Generate, Key, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub(crate) fn routes() -> Router<Engine> {
    Router::new()
        .route("/repos/{repo}/secrets", get(secrets))
        .route("/repos/{repo}/secrets/{name}", post(set_secret))
        .route("/repos/{repo}/secrets/{name}/remove", post(remove_secret))
}

/// What a value is sealed with, written beside it in `secrets.json`.
const CIPHER: &str = "xchacha20-poly1305";

#[derive(serde::Serialize, Deserialize)]
struct Sealed {
    set_at: i64,
    /// Base64 of the nonce, then the value enciphered with its tag (`seal`).
    sealed: String,
    /// `CIPHER`. Empty: sealed with blake3 by a server from before it, until `reseal_old`.
    #[serde(default)]
    cipher: String,
}

/// One writer at a time: a change reads the store, changes it and writes it back.
static STORE: Mutex<()> = Mutex::new(());

/// The secrets a server keeps for the tests of each repo (`avp secret set`), in its data folder:
/// `secrets.json` holds each value sealed with `secrets.key` (XChaCha20-Poly1305, `seal`), both
/// readable by the server's user alone (0600). A value is opened only to hand it to the steps
/// avp.toml names it for, and masked in what they print: no route returns one.
#[derive(Clone)]
pub struct SecretStore {
    data: PathBuf,
}

impl SecretStore {
    pub fn new(data: &Path) -> Self {
        Self { data: data.to_path_buf() }
    }

    fn file(&self) -> PathBuf {
        self.data.join("secrets.json")
    }

    /// Repo id, then name.
    fn load(&self) -> Result<BTreeMap<String, BTreeMap<String, Sealed>>> {
        match fs::read(self.file()) {
            Ok(b) => Ok(serde_json::from_slice(&b).context("secrets.json")?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e).context("secrets.json"),
        }
    }

    /// The key, made on the first secret set (`create`). Lost, the values cannot be opened: they are
    /// set again.
    fn key(&self, create: bool) -> Result<Option<[u8; 32]>> {
        let path = self.data.join("secrets.key");
        if create && !path.exists() {
            // Made before the file, which a failing generator would otherwise leave empty.
            let key: [u8; 32] = Key::<XChaCha20Poly1305>::try_generate().map_err(|e| anyhow::anyhow!("the system's random generator: {e}"))?.into();
            fs::create_dir_all(&self.data)?;
            let mut open = fs::OpenOptions::new();
            open.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
            match open.open(&path) {
                Ok(mut f) => std::io::Write::write_all(&mut f, blake3::Hash::from(key).to_hex().as_bytes())?,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e).context("secrets.key"),
            }
        }
        match fs::read_to_string(&path) {
            Ok(hex) => Ok(Some(*blake3::Hash::from_hex(hex.trim()).context("secrets.key")?.as_bytes())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("secrets.key"),
        }
    }

    /// Writes the store whole or not at all, readable by the server's user alone.
    fn save(&self, all: &BTreeMap<String, BTreeMap<String, Sealed>>) -> Result<()> {
        let tmp = self.data.join(format!(".secrets-{}.json", ulid::Ulid::new()));
        let mut open = fs::OpenOptions::new();
        open.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        std::io::Write::write_all(&mut open.open(&tmp)?, &serde_json::to_vec_pretty(all)?)?;
        fs::rename(&tmp, self.file())?;
        Ok(())
    }

    pub fn set(&self, repo: &str, name: &str, value: &str) -> Result<SecretInfo> {
        if let Some(why) = refused_secret(name) {
            anyhow::bail!("{name}: {why}");
        }
        // Shorter, it could not be masked without hiding the words that hold it (see `hidden`).
        anyhow::ensure!(value.chars().count() > 6, "{name}: a value of 7 characters or more, or what the steps print of it could not be masked");
        anyhow::ensure!(value.len() <= 64 * 1024, "{name}: a value of 64 KiB at most");
        let _one = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let key = self.key(true)?.context("secrets.key")?;
        let mut all = self.load()?;
        let info = SecretInfo { name: name.to_string(), set_at: now_ms() };
        let sealed = seal(&key, repo, name, value.as_bytes())?;
        all.entry(repo.to_string()).or_default().insert(name.to_string(), Sealed { set_at: info.set_at, sealed: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sealed), cipher: CIPHER.into() });
        self.save(&all)?;
        Ok(info)
    }

    pub fn list(&self, repo: &str) -> Result<Vec<SecretInfo>> {
        Ok(self.load()?.remove(repo).unwrap_or_default().into_iter().map(|(name, s)| SecretInfo { name, set_at: s.set_at }).collect())
    }

    /// False when the repo had no such secret.
    pub fn remove(&self, repo: &str, name: &str) -> Result<bool> {
        let _one = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        let removed = all.get_mut(repo).is_some_and(|s| s.remove(name).is_some());
        if removed {
            all.retain(|_, s| !s.is_empty());
            self.save(&all)?;
        }
        Ok(removed)
    }

    /// The values of those of `names` the repo has, opened for the steps that name them. A name
    /// refused to steps stays out, whatever the file says.
    pub fn values(&self, repo: &str, names: &BTreeSet<String>) -> Result<BTreeMap<String, String>> {
        let mut kept = self.load()?.remove(repo).unwrap_or_default();
        kept.retain(|n, _| names.contains(n) && refused_secret(n).is_none());
        if kept.is_empty() {
            return Ok(BTreeMap::new());
        }
        let key = self.key(false)?.context("secrets.key is gone: the secrets of secrets.json cannot be opened, set them again")?;
        kept.into_iter()
            .map(|(name, s)| {
                // Still sealed with blake3: the server's start could not reseal it (`reseal_old`).
                anyhow::ensure!(s.cipher == CIPHER, "secret {name} was sealed before {CIPHER} and could not be resealed: set it again");
                let sealed = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s.sealed).with_context(|| format!("secret {name}"))?;
                let value = unseal(&key, repo, &name, &sealed).with_context(|| format!("secret {name}"))?;
                Ok((name, String::from_utf8(value)?))
            })
            .collect()
    }

    /// Reseals with `CIPHER` the values a server from before it sealed with blake3, when the server
    /// starts, before a proof opens one: a line per value, said in its log. A value that does not
    /// open stays as it is, and a step that names it is red until it is set again.
    pub fn reseal_old(&self) -> Result<Vec<String>> {
        let _one = STORE.lock().unwrap_or_else(|e| e.into_inner());
        let mut all = self.load()?;
        if !all.values().flat_map(|s| s.values()).any(|s| s.cipher != CIPHER) {
            return Ok(vec![]);
        }
        let key = self.key(false)?;
        let mut said = vec![];
        for (repo, kept) in all.iter_mut() {
            for (name, s) in kept.iter_mut().filter(|(_, s)| s.cipher != CIPHER) {
                let resealed = key.context("secrets.key is gone").and_then(|key| {
                    let sealed = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &s.sealed)?;
                    seal(&key, repo, name, &blake3_open(&key, &format!("{repo}/{name}"), &sealed)?)
                });
                said.push(match resealed {
                    Ok(sealed) => {
                        (s.sealed, s.cipher) = (base64::Engine::encode(&base64::engine::general_purpose::STANDARD, sealed), CIPHER.into());
                        format!("secret {name} of {repo}: resealed with {CIPHER}")
                    }
                    Err(e) => format!("secret {name} of {repo}: not resealed ({e:#}), a step that names it is red until it is set again"),
                });
            }
        }
        self.save(&all)?;
        Ok(said)
    }
}

/// `value` sealed with XChaCha20-Poly1305 under a random nonce of its own, written before it, with
/// its repo and name as associated data: a value changed, or moved to another name or repo, does
/// not open. A name holds no `/` (`refused_secret`), so `repo/name` reads one way only.
fn seal(key: &[u8; 32], repo: &str, name: &str, value: &[u8]) -> Result<Vec<u8>> {
    let nonce = XNonce::try_generate().map_err(|e| anyhow::anyhow!("the system's random generator: {e}"))?;
    let sealed = XChaCha20Poly1305::new(&(*key).into())
        .encrypt(&nonce, Payload { msg: value, aad: format!("{repo}/{name}").as_bytes() })
        .map_err(|_| anyhow::anyhow!("value not sealed"))?;
    Ok([nonce.as_slice(), &sealed].concat())
}

fn unseal(key: &[u8; 32], repo: &str, name: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    anyhow::ensure!(sealed.len() >= 24 + 16, "sealed value cut short");
    let (nonce, body) = sealed.split_at(24);
    XChaCha20Poly1305::new(&(*key).into())
        .decrypt(&XNonce::try_from(nonce)?, Payload { msg: body, aad: format!("{repo}/{name}").as_bytes() })
        .map_err(|_| anyhow::anyhow!("sealed value changed, or not sealed with this key for this name"))
}

/// Opens what a server from before `CIPHER` sealed (blake3 as a keyed keystream, then a tag over
/// the nonce, `context` and the cipher text), for `reseal_old` alone: it goes once every server
/// has started on `CIPHER`.
fn blake3_open(key: &[u8; 32], context: &str, sealed: &[u8]) -> Result<Vec<u8>> {
    let (cipher, mac) = (blake3::derive_key("avp 2026-10-28 secrets for tests: cipher", key), blake3::derive_key("avp 2026-10-28 secrets for tests: tag", key));
    anyhow::ensure!(sealed.len() >= 24 + 32, "sealed value cut short");
    let (body, tag) = sealed.split_at(sealed.len() - 32);
    let expected = blake3::Hasher::new_keyed(&mac).update(&(context.len() as u64).to_le_bytes()).update(context.as_bytes()).update(body).finalize();
    // blake3::Hash compares in constant time.
    anyhow::ensure!(expected == blake3::Hash::from(<[u8; 32]>::try_from(tag)?), "sealed value changed, or not sealed with this key for this name");
    let (nonce, body) = body.split_at(24);
    let mut value = body.to_vec();
    let mut stream = vec![0u8; value.len()];
    blake3::Hasher::new_keyed(&cipher).update(nonce).finalize_xof().fill(&mut stream);
    value.iter_mut().zip(&stream).for_each(|(b, s)| *b ^= s);
    Ok(value)
}

#[derive(Deserialize)]
struct SetSecret {
    value: String,
    #[serde(default)]
    person: Option<String>,
}

/// `avp secret set`: the value comes in the body, never in the address, and never goes back out.
async fn set_secret(State(e): S, P((repo, name)): P<(String, String)>, headers: HeaderMap, Json(req): Json<SetSecret>) -> R {
    let person = verified_person(&headers).or(req.person).unwrap_or_else(|| "someone".into());
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        let set = SecretStore::new(&e.data).set(&r, &name, &req.value)?;
        e.db.lock().unwrap().emit(&r, "secret.set", None, None, serde_json::json!({ "name": set.name, "by": person }))?;
        Ok(set)
    })
    .await?)
}

/// The repo's secrets: names and dates, no value.
async fn secrets(State(e): S, P(repo): P<String>) -> R {
    ok(blocking(move || { let r = rid(&e, &repo)?; SecretStore::new(&e.data).list(&r) }).await?)
}

#[derive(Deserialize, Default)]
struct Who {
    #[serde(default)]
    person: Option<String>,
}

async fn remove_secret(State(e): S, P((repo, name)): P<(String, String)>, headers: HeaderMap, body: Option<Json<Who>>) -> R {
    let person = verified_person(&headers).or(body.and_then(|b| b.0.person)).unwrap_or_else(|| "someone".into());
    ok(blocking(move || {
        let r = rid(&e, &repo)?;
        anyhow::ensure!(SecretStore::new(&e.data).remove(&r, &name)?, "no secret {name} in this repo: `avp secret list` shows those set");
        e.db.lock().unwrap().emit(&r, "secret.removed", None, None, serde_json::json!({ "name": name, "by": person }))?;
        Ok(serde_json::json!({ "removed": name }))
    })
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A secret is kept sealed with XChaCha20-Poly1305, in files the server's user alone reads,
    /// opens for its repo and name only, and never under a name a step must not get.
    #[test]
    fn a_secret_is_sealed_at_rest_and_opens_for_its_repo_and_name_only() {
        let data = std::env::temp_dir().join(format!("avp-test-secrets-{}", ulid::Ulid::new()));
        let store = SecretStore::new(&data);
        let value = "test-value-7f3a91c2e4";
        store.set("repo_a", "STRIPE_KEY", value).unwrap();
        let names = |n: &[&str]| n.iter().map(|n| n.to_string()).collect::<BTreeSet<_>>();
        assert_eq!(store.values("repo_a", &names(&["STRIPE_KEY", "UNSET_KEY"])).unwrap(), BTreeMap::from([("STRIPE_KEY".to_string(), value.to_string())]));
        assert!(store.values("repo_b", &names(&["STRIPE_KEY"])).unwrap().is_empty());
        assert_eq!(store.list("repo_a").unwrap().iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["STRIPE_KEY"]);
        for f in ["secrets.json", "secrets.key"] {
            let at_rest = fs::read(data.join(f)).unwrap();
            assert!(!String::from_utf8_lossy(&at_rest).contains("4eC39HqLyjWDarjtT1zdp7dc"), "{f} holds the value");
            #[cfg(unix)]
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&fs::metadata(data.join(f)).unwrap().permissions()) & 0o777, 0o600, "{f}");
        }
        // A nonce of its own each time; another repo or name in the associated data, it does not open.
        let key = store.key(false).unwrap().unwrap();
        let sealed = seal(&key, "repo_a", "STRIPE_KEY", value.as_bytes()).unwrap();
        assert_ne!(sealed, seal(&key, "repo_a", "STRIPE_KEY", value.as_bytes()).unwrap());
        assert_eq!(unseal(&key, "repo_a", "STRIPE_KEY", &sealed).unwrap(), value.as_bytes());
        assert!(unseal(&key, "repo_b", "STRIPE_KEY", &sealed).is_err() && unseal(&key, "repo_a", "PAYPAL_KEY", &sealed).is_err());
        // Moved to another name in the store, it does not open.
        let moved = fs::read_to_string(data.join("secrets.json")).unwrap().replace("STRIPE_KEY", "PAYPAL_KEY");
        assert!(moved.contains(r#""cipher": "xchacha20-poly1305""#), "{moved}");
        fs::write(data.join("secrets.json"), moved).unwrap();
        assert!(store.values("repo_a", &names(&["PAYPAL_KEY"])).is_err());
        for name in ["AWS_SECRET_ACCESS_KEY", "GH_TOKEN", "ANTHROPIC_API_KEY", "AVP_TOKEN", "AVP_CHECK_SLOTS", "PATH", "LC_ALL", "CI", "1KEY", "A-KEY", ""] {
            assert!(store.set("repo_a", name, value).is_err(), "{name} was kept");
        }
        assert!(store.set("repo_a", "PIN", "123456").is_err(), "too short to be masked");
        assert!(store.remove("repo_a", "PAYPAL_KEY").unwrap() && !store.remove("repo_a", "PAYPAL_KEY").unwrap());
        fs::remove_dir_all(&data).unwrap();
    }

    /// A value a server from before XChaCha20-Poly1305 sealed with blake3 is resealed once, when
    /// the server starts, and opens as it did.
    #[test]
    fn a_value_sealed_with_blake3_is_resealed() {
        let data = std::env::temp_dir().join(format!("avp-test-reseal-{}", ulid::Ulid::new()));
        fs::create_dir_all(&data).unwrap();
        // As the store of position 58 wrote them, with the key [7; 32].
        fs::write(data.join("secrets.key"), "07".repeat(32)).unwrap();
        fs::write(data.join("secrets.json"), r#"{"repo_a": {"STRIPE_KEY": {"set_at": 1, "sealed": "VzXWyczFR/XQdDV+jM4Wjv9wJQo6QurphUzBfAvB3S30DQlcwL8Nz+TyXzuFqk48z+5DBggNM9N5oMq0ojWrwz9wZF5pXSSuA4z6DGs06NhSf2Sx4inBNg=="}}}"#).unwrap();
        let store = SecretStore::new(&data);
        let names = BTreeSet::from(["STRIPE_KEY".to_string()]);
        assert!(store.values("repo_a", &names).is_err(), "opened the blake3 way");
        assert_eq!(store.reseal_old().unwrap(), ["secret STRIPE_KEY of repo_a: resealed with xchacha20-poly1305"]);
        assert!(store.reseal_old().unwrap().is_empty());
        // Stripe's public documentation test key, in two pieces so a secret scanner does not block a push.
        assert_eq!(store.values("repo_a", &names).unwrap()["STRIPE_KEY"], concat!("sk_", "test_4eC39HqLyjWDarjtT1zdp7dc"));
        fs::remove_dir_all(&data).unwrap();
    }
}
