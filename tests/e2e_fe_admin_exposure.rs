//! #292: the query-FE role must not publish an unauthenticated admin UI.
//!
//! `serve` is the one role an operator deliberately puts on a network - it is the query-FE half of the
//! plane split, and `docker-compose.scaled.yml` starts it as `serve --dir /nest --listen 0.0.0.0:8288`.
//! It was also the one role that derived its admin credential by reading `NUTHATCH_ADMIN_TOKEN`
//! directly instead of going through `indexer::admin_enabled` / `admin_required_token`. Unset, that
//! yields `admin_token: None`, and `serve::admin_authorized` reads `None` as *"localhost bind, open"* -
//! so `--admin` on a public bind served `/_admin/` to anyone, with a 200.
//!
//! The unit tests in `serve.rs` could not catch it: they build an `AppState` and set `admin_token`
//! themselves, which asserts the handler enforces a token it was handed, not that any role hands it
//! one. This suite therefore drives **`indexer::serve_role`** - the production entry point - and asks
//! the socket, because the wiring is the thing that was wrong.
//!
//! **`127.0.0.2` is the bind on purpose.** `serve::is_localhost` matches the literal host string
//! (`127.0.0.1`, `::1`, `localhost`, `[::1]`), so `127.0.0.2` is off-localhost by the code's own
//! definition while still being loopback - the off-localhost path gets exercised without this suite
//! ever opening a port to the network.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use nuthatch::cli::ServeArgs;

mod common;

use common::tape::{scaffold_nest, USDC};

const POLL_TIMEOUT: Duration = Duration::from_secs(20);

/// `NUTHATCH_ADMIN_TOKEN` is process-wide and both tests below depend on its value, so they take turns
/// rather than racing. Held across each test body, not just the write.
static ENV: Mutex<()> = Mutex::new(());

/// A nest `serve_role` can load: `scaffold_nest` writes `rpc_urls = []`, and the FE constructs an
/// `RpcClient` it never polls, which refuses an empty URL list.
fn scaffold_fe_nest(dir: &std::path::Path) -> nuthatch::config::Config {
    scaffold_nest(dir, "usdc", USDC);
    let toml_path = dir.join("nuthatch.toml");
    let toml = std::fs::read_to_string(&toml_path).unwrap();
    // Never dialled - the FE owns no cursor. It just has to parse.
    std::fs::write(
        &toml_path,
        toml.replace("rpc_urls = []", r#"rpc_urls = ["http://127.0.0.1:1"]"#),
    )
    .unwrap();
    nuthatch::config::Config::load(dir).unwrap()
}

/// A free port on `127.0.0.2`, released before we hand it to the FE.
async fn free_port() -> u16 {
    let probe = tokio::net::TcpListener::bind("127.0.0.2:0").await.unwrap();
    probe.local_addr().unwrap().port()
}

/// Start `serve_role` on `127.0.0.2:<port>` with the admin UI requested, and wait until it answers.
/// Returns the base URL and the task, so the caller can abort it.
async fn start_fe(dir: &std::path::Path, admin: bool) -> (String, tokio::task::JoinHandle<()>) {
    let port = free_port().await;
    let listen = format!("127.0.0.2:{port}");
    let args = ServeArgs {
        dir: dir.to_string_lossy().into_owned(),
        listen: listen.clone(),
        hot_store: None,
        admin,
    };
    let task = tokio::spawn(async move {
        // Runs until the process is signalled; the test aborts it instead.
        let _ = nuthatch::indexer::serve_role(args).await;
    });

    let base = format!("http://{listen}");
    let client = reqwest::Client::new();
    let start = Instant::now();
    while start.elapsed() < POLL_TIMEOUT {
        if let Ok(resp) = client.get(&base).send().await {
            if resp.status().is_success() {
                return (base, task);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("FE never came up on {listen}");
}

/// The defect, stated as an assertion: `--admin` on an off-localhost bind with no token configured must
/// not serve the admin UI. Reverting `serve_role` to `admin_token_env()` turns this red with a 200.
#[tokio::test]
async fn the_fe_admin_ui_is_not_open_on_an_off_localhost_bind() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    // Removing it makes the "no token configured" premise true regardless of the developer's shell,
    // rather than the test quietly asserting something else on a machine that exports one.
    std::env::remove_var("NUTHATCH_ADMIN_TOKEN");

    let dir = tempfile::tempdir().unwrap();
    scaffold_fe_nest(dir.path());
    let (base, task) = start_fe(dir.path(), true).await;

    let status = reqwest::Client::new()
        .get(format!("{base}/_admin/"))
        .send()
        .await
        .expect("admin request")
        .status();
    task.abort();

    assert_eq!(
        status.as_u16(),
        404,
        "an off-localhost FE with no NUTHATCH_ADMIN_TOKEN must not serve /_admin/ at all; got {status}"
    );
}

/// The other half, so the fix cannot be "disable the admin UI on the FE and call it secure": with a
/// token configured the surface is live and *gated*, which is what an operator behind a tunnel wants.
#[tokio::test]
async fn a_configured_token_gates_the_fe_admin_ui_rather_than_disabling_it() {
    let _env = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("NUTHATCH_ADMIN_TOKEN", "s3cret");

    let dir = tempfile::tempdir().unwrap();
    scaffold_fe_nest(dir.path());
    let (base, task) = start_fe(dir.path(), true).await;

    let client = reqwest::Client::new();
    let without = client
        .get(format!("{base}/_admin/"))
        .send()
        .await
        .expect("admin request")
        .status();
    let with = client
        .get(format!("{base}/_admin/?token=s3cret"))
        .send()
        .await
        .expect("admin request")
        .status();
    task.abort();
    std::env::remove_var("NUTHATCH_ADMIN_TOKEN");

    assert_eq!(without.as_u16(), 401, "no credential must be refused");
    assert_eq!(with.as_u16(), 200, "the configured token must be accepted");
}
