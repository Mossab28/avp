//! AVP (agent versioning protocol): version control for agents. The server holds the truth; a task
//! replaces the branch, the commit and the pull request; a proof replaces the CI run.

pub mod blame;
pub mod check;
pub mod checkout;
pub mod client;
pub mod db;
pub mod engine;
pub mod gitbridge;
pub mod merge;
pub mod history;
pub mod importci;
pub mod mcp;
pub mod migrate;
pub mod model;
pub mod objects;
pub mod offline;
pub mod seam;
pub mod server;
pub mod stats;
pub mod up;
pub mod view;
pub mod wait;
pub mod worker;

// AVP Cloud, a team's hosted server: the core reaches it through `seam` alone.
#[cfg(feature = "cloud")]
pub mod cloud;

/// What this binary runs on, as `avp update` compares it with the server's: a client takes the
/// server's binary only when both run on the same platform.
pub fn platform() -> String {
    let libc = if cfg!(target_env = "musl") { "-musl" } else if cfg!(target_os = "linux") { "-gnu" } else { "" };
    format!("{}-{}{libc}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The file this process runs: on Linux /proc/self/exe, still there if the file was replaced since.
pub fn own_exe() -> std::io::Result<std::path::PathBuf> {
    if cfg!(target_os = "linux") {
        Ok(std::path::PathBuf::from("/proc/self/exe"))
    } else {
        std::env::current_exe()
    }
}

/// Carries the environment's former names (TRAME_SERVER, TRAME_WORKERS...) over to AVP_*, for a
/// machine set up before AVP was named AVP. Called first thing, before any thread starts.
pub fn legacy_env() {
    for (k, v) in std::env::vars() {
        if let Some(rest) = k.strip_prefix("TRAME_") {
            let new = format!("AVP_{rest}");
            if std::env::var_os(&new).is_none() {
                std::env::set_var(new, v);
            }
        }
    }
}
