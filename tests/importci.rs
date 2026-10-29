//! `avp import-ci` on three CIs (tests/import-ci): a Node monorepo on GitHub Actions with services,
//! a matrix, a secret, deploys and a schedule; a Python repo on a cron AVP cannot say; a GitLab file.

use avp::check::{AvpConfig, Deploy, Ready, ServiceKind};
use avp::importci::Imported;
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/import-ci").join(name)
}

fn import(dir: &Path) -> (Imported, AvpConfig) {
    let i = avp::importci::import(dir).unwrap();
    let cfg: AvpConfig = toml::from_str(&i.toml).unwrap();
    (i, cfg)
}

fn noted(i: &Imported, at: &str, what: &str) -> bool {
    i.not_translated.iter().any(|n| n.at == at && n.what.contains(what))
}

fn steps(cfg: &AvpConfig) -> Vec<(&str, &str)> {
    cfg.check.steps.iter().map(|s| (s.name.as_str(), s.run.as_str())).collect()
}

#[test]
fn a_node_monorepo_brings_its_proof_its_services_its_matrix_and_its_deploys() {
    let (i, cfg) = import(&fixture("node-monorepo"));
    let c = &cfg.check;
    assert_eq!(c.setup.as_deref(), Some("pnpm install --frozen-lockfile"));
    assert_eq!(c.setup_keys, ["package.json", "pnpm-lock.yaml", "pnpm-workspace.yaml"]);
    let s = steps(&cfg);
    assert_eq!(s[..2], [("typecheck", "pnpm typecheck"), ("lint", "pnpm lint")]);
    assert!(s[2].1.ends_with("node_modules/.bin/vitest related --run --passWithNoTests {files}") && c.steps[2].needs_files, "{s:?}");
    assert_eq!(s.len(), 3, "the e2e tests stay in the full suite: {s:?}");

    // One shard per job; the matrix only where a command reads it, the install left to the setup.
    let shards = c.shards();
    let names: Vec<&str> = shards.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["lint", "test (shard 1)", "test (shard 2)", "e2e", "nightly-e2e"]);
    let test = &shards[1];
    assert!(test.run.contains("pnpm test -- --shard=${MATRIX_SHARD}/2") && !test.run.contains("pnpm install"), "{}", test.run);
    assert_eq!(test.services, None, "it needs every service");
    assert_eq!(shards[0].services, Some(vec![]), "the lint starts none");

    // `secrets.NPM_TOKEN` is AVP's to keep: named where it is read, set once by a person.
    assert_eq!((test.secrets.as_slice(), c.steps[2].secrets.as_slice(), c.steps[0].secrets.as_slice()), (&["NPM_TOKEN".to_string()][..], &["NPM_TOKEN".to_string()][..], &[][..]));
    assert_eq!(i.secrets, ["NPM_TOKEN"]);
    assert!(avp::importci::report(&i).ends_with("#   avp secret set NPM_TOKEN\n"), "{}", avp::importci::report(&i));
    assert!(!i.not_translated.iter().any(|n| n.what.contains("NPM_TOKEN")), "{:?}", i.not_translated);

    // The nightly workflow's cron, as the time of day its shard runs at.
    let nightly: Vec<(&str, &[String])> = c.schedule.iter().map(|s| (s.at.as_str(), s.shards.as_slice())).collect();
    assert_eq!(nightly, [("03:00", &["nightly-e2e".to_string()][..])]);
    assert!(noted(&i, "nightly.yml, job e2e", "runs only on a schedule"), "a shard runs after each batch too: {:?}", i.not_translated);

    let pg = c.services.iter().find(|s| s.name == "postgres").unwrap();
    assert_eq!(pg.kind, ServiceKind::Image { image: "postgres:16".into(), ports: vec![5432] });
    assert_eq!(pg.ready, Some(Ready::Command("pg_isready".into())));
    // The tests reached it on localhost:5432; on AVP each proof has its own, on the port it picks.
    assert!(pg.env.contains(&("DATABASE_URL".into(), "postgres://postgres:postgres@127.0.0.1:{port}/shop_test".into())), "{:?}", pg.env);
    assert!(c.services.iter().any(|s| s.name == "redis"));

    let staging = &cfg.envs["staging"];
    assert_eq!(staging.follow.as_deref(), Some("full"), "it deployed on every push");
    let gh = staging.deploy.as_ref().and_then(Deploy::github).unwrap();
    assert_eq!((gh.github_workflow.as_str(), gh.inputs.get("sha").map(String::as_str)), ("deploy-staging.yml", Some("{git}")));
    let production = &cfg.envs["production"];
    assert_eq!((production.after.as_deref(), production.follow.as_deref()), (Some("staging"), None));
    assert_eq!(production.deploy.as_ref().and_then(Deploy::github).unwrap().github_workflow, "deploy-production.yml");

    assert!(noted(&i, "ci.yml, job test", "matrix `node`"));
    assert!(noted(&i, "ci.yml, job e2e", "actions/upload-artifact"));
    assert!(noted(&i, "ci.yml, job e2e", "slackapi/slack-github-action"));
    // A deploy runs in GitHub: its actions and secrets stay there, nothing to translate.
    assert!(!i.not_translated.iter().any(|n| n.at.starts_with("deploy-")), "{:?}", i.not_translated);
}

#[test]
fn a_python_repo_proves_from_a_virtualenv() {
    let (i, cfg) = import(&fixture("python"));
    let c = &cfg.check;
    assert_eq!(c.setup.as_deref(), Some("python3 -m venv .venv && .venv/bin/pip install -q -r requirements.txt -r requirements-dev.txt"));
    assert_eq!((c.setup_keys.as_slice(), c.setup_links.as_slice()), (&["requirements.txt".to_string(), "requirements-dev.txt".into()][..], &[".venv".to_string()][..]));
    // The project itself installs in the candidate, before its first Python step.
    assert_eq!(steps(&cfg), [("typecheck", ".venv/bin/pip install -q -e . && .venv/bin/python -m mypy src"), ("lint", ".venv/bin/python -m ruff check ."), ("tests", ".venv/bin/python -m pytest -q --cov=app")]);
    let shards = c.shards();
    assert_eq!(shards.len(), 1, "python-version only sets up the runner: one shard");
    assert!(!shards[0].run.contains("-r requirements") && !shards[0].run.contains("--upgrade pip"), "{}", shards[0].run);
    assert!(noted(&i, "tests.yml, job test", "codecov/codecov-action"));
    assert!(noted(&i, "tests.yml, job test", "macos-latest"));
    assert!(cfg.envs.is_empty() && c.services.is_empty());
    // The codecov action is left out, and the token only it read with it.
    assert!(i.secrets.is_empty() && shards[0].secrets.is_empty(), "{:?}", i.secrets);

    // `30 5,17 * * 1-5`: twice a day on weekdays, which no schedule of AVP says. Its first time, every
    // day, listed with why.
    assert_eq!(c.schedule.iter().map(|s| (s.at.as_str(), s.shards.clone())).collect::<Vec<_>>(), [("05:30", vec!["test".to_string()])]);
    let n = i.not_translated.iter().find(|n| n.at == "tests.yml" && n.what.contains("30 5,17 * * 1-5")).unwrap();
    assert!(n.what.contains("several times a day and on some days only") && n.todo.contains("every day, at 05:30 UTC") && n.todo.contains("add one per time of day"), "{n:?}");
    assert!(!noted(&i, "tests.yml, job test", "runs only on a schedule"), "it runs on a push too");
}

#[test]
fn a_gitlab_file_brings_its_templates_services_matrix_and_deploys() {
    let (i, cfg) = import(&fixture("gitlab"));
    let c = &cfg.check;
    assert_eq!(c.setup.as_deref(), Some("npm ci"));
    let s = steps(&cfg);
    assert_eq!(s[..2], [("typecheck", "npx tsc --noEmit"), ("lint", "npm run lint")]);
    assert!(s[2].1.ends_with("node_modules/.bin/jest --findRelatedTests --passWithNoTests {files}") && !s[2].1.contains("SUITE"), "{s:?}");

    let names: Vec<String> = c.shards().into_iter().map(|s| s.name).collect();
    assert_eq!(names, ["lint", "test (SUITE unit)", "test (SUITE api)", "audit", "nightly"]);
    // GitLab keeps its schedules' times out of the file: GitLab's every-day preset, said.
    assert_eq!(c.schedule.iter().map(|s| (s.at.as_str(), s.shards.clone())).collect::<Vec<_>>(), [("04:00", vec!["nightly".to_string()])]);
    assert!(i.not_translated.iter().any(|n| n.at == ".gitlab-ci.yml" && n.what.contains("CI/CD > Schedules") && n.todo.contains("04:00")), "{:?}", i.not_translated);
    assert!(noted(&i, ".gitlab-ci.yml, job nightly", "runs only on a schedule"));
    let test = c.full_shards.iter().find(|s| s.name == "test").unwrap();
    assert!(test.run.contains("SUITE=\"$MATRIX_SUITE\"") && !test.run.contains("npm ci"), "{}", test.run);
    // `<<: *with-db` and `extends: .with-db` both bring the database.
    assert_eq!((test.services.clone(), c.full_shards.iter().find(|s| s.name == "audit").unwrap().services.clone()), (None, None));

    let pg = &c.services[0];
    assert_eq!(pg.kind, ServiceKind::Image { image: "postgres:16".into(), ports: vec![5432] });
    assert!(pg.env.contains(&("DATABASE_URL".into(), "postgres://postgres:secret@127.0.0.1:{port}/billing".into())), "reached as `postgres` on GitLab: {:?}", pg.env);
    assert!(pg.env.contains(&("POSTGRES_DB".into(), "billing".into())));

    let Some(Deploy::Run(staging)) = &cfg.envs["staging"].deploy else { panic!("{:?}", cfg.envs) };
    assert!(staging.contains("./scripts/deploy.sh staging {git}") && !staging.contains("DATABASE_URL"), "{staging}");
    assert_eq!(cfg.envs["staging"].follow.as_deref(), Some("full"));
    assert_eq!((cfg.envs["production"].after.as_deref(), cfg.envs["production"].follow.as_deref()), (Some("staging"), None), "when: manual");

    assert!(noted(&i, ".gitlab-ci.yml", "includes `Security/SAST.gitlab-ci.yml`"));
    // A CI/CD variable a check reads is a secret of its shard; a deploy's stays the server's.
    let audit = c.full_shards.iter().find(|s| s.name == "audit").unwrap();
    assert_eq!((audit.secrets.as_slice(), i.secrets.as_slice()), (&["PRIVATE_REGISTRY".to_string()][..], &["PRIVATE_REGISTRY".to_string()][..]));
    assert!(!i.not_translated.iter().any(|n| n.what.contains("$PRIVATE_REGISTRY")), "{:?}", i.not_translated);
    assert!(i.not_translated.iter().any(|n| n.what.contains("$SLACK_WEBHOOK") && n.todo.contains("AVP_DEPLOY_ENV")));
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        if e.file_type().unwrap().is_dir() {
            copy(&e.path(), &to.join(e.file_name()));
        } else {
            std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
        }
    }
}

#[test]
fn write_never_overwrites() {
    let dir = std::env::temp_dir().join(format!("avp-import-ci-{}", ulid::Ulid::new()));
    copy(&fixture("python"), &dir);
    let i = avp::importci::import(&dir).unwrap();
    assert_eq!(avp::importci::write(&dir, &i).unwrap(), dir.join("avp.toml"));
    std::fs::write(dir.join("avp.toml"), "# the team's own\n").unwrap();
    // A repo with its avp.toml gets the proposal beside it.
    assert_eq!(avp::importci::write(&dir, &avp::importci::import(&dir).unwrap()).unwrap(), dir.join("avp.imported.toml"));
    let again = avp::importci::write(&dir, &i).unwrap_err().to_string();
    assert!(again.contains("avp.imported.toml is already there"), "{again}");
    assert_eq!(std::fs::read_to_string(dir.join("avp.toml")).unwrap(), "# the team's own\n");
    assert_eq!(std::fs::read_to_string(dir.join("avp.imported.toml")).unwrap(), i.toml);
    let _ = std::fs::remove_dir_all(&dir);
}
