//! A repo's settings from the CLI (`avp repo ...`) and the avp_repo tool, one mapping for both: an
//! action and its arguments become the request the settings page sends, and the server's answer
//! says what changed and what to do next (`said`).

use crate::client::Client;
use anyhow::{anyhow, Result};
use serde_json::{json, Value};

/// The actions, as the tool lists them.
pub const ACTIONS: &[&str] = &["show", "invite", "role", "remove", "token", "revoke", "rename", "describe", "visibility", "transfer", "archive", "unarchive", "delete", "restore", "log"];

/// Sends an action on `repo`'s settings. `args` carries what it needs: `person` and `role` (invite,
/// role, remove), `name`, `role` and `expires` (token), `id` (revoke), `name` (rename),
/// `description`, `visibility`, `to` (transfer), `confirm` (delete), `before` and `limit` (log).
pub fn ask(client: &Client, repo: &str, action: &str, args: &Value) -> Result<Value> {
    let at = |rest: &str| format!("/repos/{repo}/settings{rest}");
    let s = |k: &str| args[k].as_str().map(str::trim).unwrap_or_default().to_string();
    let need = |k: &str| -> Result<String> { Some(s(k)).filter(|v| !v.is_empty()).ok_or_else(|| anyhow!("{action} needs `{k}`")) };
    match action {
        "show" => client.get(&at("")),
        "invite" | "role" => client.post(&at("/collaborators"), &json!({ "person": need("person")?, "role": Some(s("role")).filter(|r| !r.is_empty()).unwrap_or_else(|| "write".into()) })),
        "remove" => client.post(&at("/collaborators"), &json!({ "person": need("person")?, "role": "none" })),
        "token" => client.post(&at("/tokens"), &json!({ "name": s("name"), "role": Some(s("role")).filter(|r| !r.is_empty()).unwrap_or_else(|| "read".into()), "expires": Some(s("expires")).filter(|r| !r.is_empty()).unwrap_or_else(|| "never".into()) })),
        "revoke" => client.post(&at(&format!("/tokens/{}/revoke", need("id")?)), &json!({})),
        "rename" => client.post(&at("/general"), &json!({ "name": need("name")? })),
        "describe" => client.post(&at("/general"), &json!({ "description": s("description") })),
        "visibility" => client.post(&at("/general"), &json!({ "visibility": need("visibility")? })),
        "transfer" => client.post(&at("/transfer"), &json!({ "to": need("to")? })),
        "archive" | "unarchive" => client.post(&at("/archive"), &json!({ "archived": action == "archive" })),
        "delete" => client.post(&at("/delete"), &json!({ "confirm": s("confirm") })),
        "restore" => client.post(&at("/restore"), &json!({})),
        "log" => {
            let mut q = format!("?limit={}", args["limit"].as_i64().unwrap_or(50));
            if let Some(b) = args["before"].as_i64() {
                q.push_str(&format!("&before={b}"));
            }
            client.get(&at(&format!("/audit{q}")))
        }
        other => Err(anyhow!("{other:?} is no repo action: {}", ACTIONS.join(", "))),
    }
}

/// The answer for a person or an agent: a few lines, ending on what to do next.
pub fn text(action: &str, v: &Value) -> String {
    let utc = |k: &Value| k.as_i64().map(crate::blame::utc).unwrap_or_else(|| "never".into());
    match action {
        "show" => {
            let mut out = vec![v["said"].as_str().unwrap_or_default().to_string()];
            if let Some(d) = v["description"].as_str().filter(|d| !d.is_empty()) {
                out.push(format!("description: {d}"));
            }
            out.push("who reaches it:".into());
            for c in v["collaborators"].as_array().into_iter().flatten() {
                let via = c["org_role"].as_str().map(|o| format!(" ({o} of {})", v["org"].as_str().unwrap_or_default())).unwrap_or_default();
                out.push(format!("  {:<20} {:<6}{via}", c["person"].as_str().unwrap_or_default(), c["role"].as_str().unwrap_or_default()));
            }
            out.push("tokens that reach it:".into());
            for t in v["tokens"].as_array().into_iter().flatten() {
                out.push(format!("  {}  {:<16} {:<12} {}, last used {}", t["id"].as_str().unwrap_or_default(), t["name"].as_str().unwrap_or_default(), t["person"].as_str().unwrap_or_default(), t["scope"].as_str().unwrap_or_default(), utc(&t["used_at"])));
            }
            let sec = &v["security"];
            let list = |k: &str| sec[k].as_array().map(|a| a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
            let guarded = [list("always_guarded"), list("guarded")].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ");
            out.push(format!("guarded: {guarded}; approved by {}", Some(list("approvers")).filter(|a| !a.is_empty()).unwrap_or_else(|| "no one yet".into())));
            out.join("\n")
        }
        "token" => format!("{}\n{}", v["token"].as_str().unwrap_or_default(), v["said"].as_str().unwrap_or_default()),
        "log" => {
            let evs = v.as_array().cloned().unwrap_or_default();
            if evs.is_empty() {
                return "no change of its settings yet".into();
            }
            evs.iter().map(|e| format!("#{:<6} {} UTC  {} {}", e["id"].as_i64().unwrap_or_default(), utc(&e["at"]), e["by"].as_str().unwrap_or("someone"), e["what"].as_str().unwrap_or_default())).collect::<Vec<_>>().join("\n")
        }
        _ => v["said"].as_str().map(String::from).unwrap_or_else(|| v.to_string()),
    }
}
