//! A file saved from AVP Cloud's website: its bytes sent whole to the task (a browser has no blake3
//! to name the object first), by a writer of the repo only, an executable staying one, and
//! avp.toml waiting for a person as it does from the CLI.
#![cfg(feature = "cloud")]

use avp::engine::Engine;
use avp::model::*;
use reqwest::blocking::{Client, RequestBuilder};
use serde_json::{json, Value};

#[test]
fn a_browser_writes_a_tasks_files_with_their_bytes() {
    let dir = std::env::temp_dir().join(format!("avp-test-{}", ulid::Ulid::new()));
    let e = Engine::open(&dir.join("data")).unwrap();
    let r = e.create_repo("web", LeasePolicy::Advisory).unwrap().id;
    let put = |s: &str| e.objects.put(s.as_bytes()).unwrap();
    let files = [
        ImportEntry { path: "avp.toml".into(), blob: put("[check]\n"), mode: 0o100644 },
        ImportEntry { path: "run.sh".into(), blob: put("echo hi\n"), mode: 0o100755 },
    ];
    e.import(&r, &files, "imported").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    rt.spawn(async move { axum::serve(listener, avp::server::router(e).into_make_service_with_connect_info::<std::net::SocketAddr>()).await });

    // As from another machine: the server's own has every right.
    let http = Client::new();
    let ask = |r: RequestBuilder, token: &str| r.header("x-forwarded-for", "203.0.113.7").bearer_auth(token);
    let json_of = |r: RequestBuilder| -> Value { r.send().unwrap().json().unwrap() };
    let token = |v: Value| v["token"]["token"].as_str().unwrap().to_string();
    let ada = token(json_of(http.post(format!("{base}/users")).header("x-forwarded-for", "203.0.113.7").json(&json!({ "name": "ada" }))));
    let bob = token(json_of(ask(http.post(format!("{base}/users")), &ada).json(&json!({ "name": "bob" }))));
    ask(http.post(format!("{base}/repos/web/roles")), &ada).json(&json!({ "person": "bob", "role": "read" })).send().unwrap();

    let task = json_of(ask(http.post(format!("{base}/repos/web/tasks")), &ada).json(&json!({ "goal": "From the browser", "actor": { "person": "", "agent": "", "session": "" } })));
    let id = task["id"].as_str().unwrap();
    let file = |path: &str, token: &str, body: &str| ask(http.put(format!("{base}/repos/web/tasks/{id}/files/{path}")), token).body(body.to_string()).send().unwrap();
    assert_eq!(file("src/a.txt", &bob, "no").status().as_u16(), 403, "reading the repo does not write it");

    let w: Value = file("src/a.txt", &ada, "hello\n").json().unwrap();
    assert_eq!((w["write"]["path"].as_str(), w["write"]["mode"].as_u64()), (Some("src/a.txt"), Some(0o100644)), "{w}");
    let w: Value = file("run.sh", &ada, "echo bye\n").json().unwrap();
    assert_eq!(w["write"]["mode"].as_u64(), Some(0o100755), "an executable edited stays one: {w}");
    let tree = json_of(ask(http.get(format!("{base}/repos/web/tasks/{id}/tree")), &ada));
    assert_eq!(tree["src/a.txt"]["blob"].as_str().unwrap(), avp::objects::hash_bytes(b"hello\n"));

    file("avp.toml", &ada, "[check]\nsteps = []\n");
    let landed = ask(http.post(format!("{base}/repos/web/tasks/{id}/land")), &ada).send().unwrap();
    assert_eq!(landed.status().as_u16(), 409);
    let landed: Value = landed.json().unwrap();
    assert_eq!((landed["reason"].as_str(), &landed["guarded"]), (Some("approval"), &json!(["avp.toml"])), "{landed}");
    let _ = std::fs::remove_dir_all(&dir);
}
