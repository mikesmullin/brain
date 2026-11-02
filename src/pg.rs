//! pg.rs — the shared PostgreSQL instance.
//!
//! brain1 embedded pglite (a single-threaded WASM Postgres inside one Bun
//! process). brain2 talks to a real server: one rootless podman container,
//! driven either straight from this CLI or as a `systemd --user` service, and
//! shared by every app on the machine. It is deliberately **not enabled by
//! default** — nothing starts a database behind your back.
//!
//! One server, many brains: each brain owns a PostgreSQL *schema* named after
//! its alias, so two brains (or brain and some other app) can never collide in
//! the namespace even if they both want a table called `entities`.
//!
//!   brain pg install    write the systemd --user unit (does not start it)
//!   brain pg start      start the container (or the unit, if installed)
//!   brain pg status     container state, server version, size per schema
//!   brain pg stop       stop it
//!   brain pg psql       interactive shell on the shared database

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::config::{self, PgConfig, DEFAULT_CONTAINER, DEFAULT_IMAGE};

pub const VOLUME: &str = "brain-pgdata";
pub const UNIT_NAME: &str = "brain-postgres.service";

fn image() -> String {
    std::env::var("BRAIN_PG_IMAGE").unwrap_or_else(|_| DEFAULT_IMAGE.to_string())
}

fn container() -> String {
    std::env::var("BRAIN_PG_CONTAINER").unwrap_or_else(|_| DEFAULT_CONTAINER.to_string())
}

fn password(cfg: &PgConfig) -> String {
    cfg.password.clone().unwrap_or_else(|| "brain".to_string())
}

pub fn unit_path() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| config::home().join(".config"));
    base.join("systemd").join("user").join(UNIT_NAME)
}

fn podman(args: &[&str]) -> Result<std::process::Output> {
    Command::new("podman")
        .args(args)
        .output()
        .context("running podman (is it installed?)")
}

fn podman_ok(args: &[&str]) -> Result<String> {
    let out = podman(args)?;
    if !out.status.success() {
        bail!(
            "podman {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// `running` / `exited` / `created` / `absent`
pub fn container_state() -> String {
    match podman(&["inspect", "-f", "{{.State.Status}}", &container()]) {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => "absent".to_string(),
    }
}

pub fn is_running() -> bool {
    container_state() == "running"
}

/// The `podman run` argv, shared by the CLI path and the systemd unit, so the
/// two can never drift into starting different servers.
///
/// The tuning is sized for the Panama Papers run (2M entities / 3.3M edges) on
/// a workstation: a big shared_buffers, generous maintenance_work_mem so the
/// GIN build does not spill, and parallelism left on.
pub fn run_argv(cfg: &PgConfig) -> Vec<String> {
    let pw = password(cfg);
    let mut v: Vec<String> = vec![
        "run".into(),
        "--name".into(),
        container(),
        "--replace".into(),
        "--rm".into(),
        "-p".into(),
        format!("127.0.0.1:{}:5432", cfg.port),
        "-e".into(),
        format!("POSTGRES_USER={}", cfg.user),
        "-e".into(),
        format!("POSTGRES_PASSWORD={pw}"),
        "-e".into(),
        format!("POSTGRES_DB={}", cfg.dbname),
        "-e".into(),
        "POSTGRES_INITDB_ARGS=--data-checksums".into(),
        // PGDATA is pinned to a SUBDIRECTORY of the mount point on purpose.
        // PostgreSQL 18's image moved its default to /var/lib/postgresql/18/docker,
        // so a mount at the old path silently produced a server with no volume
        // behind it; and initdb refuses a mount point that already holds
        // lost+found. An explicit subdirectory is correct on every version.
        "-e".into(),
        "PGDATA=/var/lib/postgresql/data/pgdata".into(),
        "-v".into(),
        format!("{VOLUME}:/var/lib/postgresql/data"),
        "--shm-size=2g".into(),
        "--health-cmd".into(),
        format!("pg_isready -U {} -d {}", cfg.user, cfg.dbname),
        "--health-interval=2s".into(),
        "--health-retries=30".into(),
        image(),
    ];
    for kv in [
        "shared_buffers=4GB",
        "effective_cache_size=24GB",
        "work_mem=256MB",
        "maintenance_work_mem=4GB",
        "max_wal_size=8GB",
        "checkpoint_timeout=30min",
        "random_page_cost=1.1",
        "effective_io_concurrency=200",
        "max_worker_processes=32",
        "max_parallel_workers=32",
        "max_parallel_workers_per_gather=8",
        "max_parallel_maintenance_workers=8",
        "jit=off",
        "synchronous_commit=off",
    ] {
        v.push("-c".into());
        v.push(kv.into());
    }
    v
}

pub fn pull(quiet: bool) -> Result<()> {
    let img = image();
    if podman(&["image", "exists", &img])?.status.success() {
        return Ok(());
    }
    if !quiet {
        eprintln!("pulling {img} …");
    }
    let st = Command::new("podman")
        .args(["pull", &img])
        .stdout(if quiet { Stdio::null() } else { Stdio::inherit() })
        .stderr(if quiet { Stdio::null() } else { Stdio::inherit() })
        .status()
        .context("podman pull")?;
    if !st.success() {
        bail!("could not pull {img}");
    }
    Ok(())
}

fn unit_installed() -> bool {
    unit_path().exists()
}

fn systemctl(args: &[&str]) -> Result<std::process::Output> {
    let mut a = vec!["--user"];
    a.extend_from_slice(args);
    Command::new("systemctl").args(&a).output().context("running systemctl --user")
}

pub fn start(cfg: &PgConfig, wait: bool) -> Result<()> {
    if is_running() {
        return Ok(());
    }
    pull(false)?;
    if unit_installed() {
        let out = systemctl(&["start", UNIT_NAME])?;
        if !out.status.success() {
            bail!(
                "systemctl --user start {UNIT_NAME} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    } else {
        let argv = run_argv(cfg);
        let mut args: Vec<&str> = vec!["-d"];
        // `-d` has to come before `run`'s own flags, so rebuild with it inline
        args.clear();
        let mut owned: Vec<String> = vec!["run".into(), "-d".into()];
        owned.extend(argv.into_iter().skip(1));
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        podman_ok(&refs)?;
    }
    if wait {
        wait_ready(cfg, 60)?;
    }
    Ok(())
}

pub fn stop() -> Result<()> {
    if unit_installed() {
        let out = systemctl(&["stop", UNIT_NAME])?;
        if out.status.success() {
            return Ok(());
        }
    }
    if container_state() == "absent" {
        return Ok(());
    }
    podman_ok(&["stop", "-t", "20", &container()])?;
    Ok(())
}

/// Poll `pg_isready` inside the container until it answers or we give up.
pub fn wait_ready(cfg: &PgConfig, secs: u64) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let c = container();
    let user = cfg.user.clone();
    let db = cfg.dbname.clone();
    loop {
        if let Ok(o) = podman(&["exec", &c, "pg_isready", "-U", &user, "-d", &db]) {
            if o.status.success() {
                return Ok(());
            }
        }
        if std::time::Instant::now() > deadline {
            bail!("postgres did not become ready within {secs}s (try `brain pg logs`)");
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}

pub fn logs(lines: usize) -> Result<String> {
    let n = lines.to_string();
    podman_ok(&["logs", "--tail", &n, &container()])
}

pub fn install_unit(cfg: &PgConfig) -> Result<PathBuf> {
    let p = unit_path();
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    let argv = run_argv(cfg)
        .into_iter()
        .map(|a| if a.contains(' ') { format!("'{a}'") } else { a })
        .collect::<Vec<_>>()
        .join(" \\\n    ");
    let unit = format!(
        "# {UNIT_NAME} — generated by `brain pg install`.\n\
         # One shared PostgreSQL for every brain (and every other app) on this machine.\n\
         # Deliberately NOT enabled: `systemctl --user enable --now {UNIT_NAME}` when you want it at login.\n\
         [Unit]\n\
         Description=brain — shared PostgreSQL (podman, pgvector)\n\
         Documentation=https://github.com/mikesmullin/brain\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         TimeoutStartSec=300\n\
         TimeoutStopSec=90\n\
         Environment=PODMAN_SYSTEMD_UNIT=%n\n\
         ExecStartPre=-/usr/bin/podman rm -f {container}\n\
         ExecStart=/usr/bin/podman {argv}\n\
         ExecStop=/usr/bin/podman stop -t 20 {container}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        container = container(),
    );
    std::fs::write(&p, unit).with_context(|| format!("writing {}", p.display()))?;
    let _ = systemctl(&["daemon-reload"]);
    Ok(p)
}

pub fn uninstall_unit() -> Result<bool> {
    let p = unit_path();
    if !p.exists() {
        return Ok(false);
    }
    let _ = systemctl(&["disable", UNIT_NAME]);
    std::fs::remove_file(&p)?;
    let _ = systemctl(&["daemon-reload"]);
    Ok(true)
}

pub fn enable(now: bool) -> Result<()> {
    if !unit_installed() {
        bail!("no unit installed — run `brain pg install` first");
    }
    let mut args = vec!["enable"];
    if now {
        args.push("--now");
    }
    args.push(UNIT_NAME);
    let out = systemctl(&args)?;
    if !out.status.success() {
        bail!("systemctl --user enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn disable() -> Result<()> {
    let out = systemctl(&["disable", UNIT_NAME])?;
    if !out.status.success() {
        bail!("systemctl --user disable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

pub fn unit_state() -> String {
    if !unit_installed() {
        return "not installed".into();
    }
    let active = systemctl(&["is-active", UNIT_NAME])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let enabled = systemctl(&["is-enabled", UNIT_NAME])
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    format!("{active} ({enabled})")
}

/// Hand off to an interactive `psql` inside the container.
pub fn psql(cfg: &PgConfig, schema: &str, sql: Option<&str>) -> Result<i32> {
    if !is_running() {
        bail!("postgres is not running — `brain pg start`");
    }
    let c = container();
    let search = format!("SET search_path TO {schema}, public;");
    let mut args: Vec<String> = vec![
        "exec".into(),
        "-it".into(),
        "-e".into(),
        format!("PGOPTIONS=-c search_path={schema},public"),
        c,
        "psql".into(),
        "-U".into(),
        cfg.user.clone(),
        "-d".into(),
        cfg.dbname.clone(),
    ];
    if let Some(s) = sql {
        args.push("-c".into());
        args.push(format!("{search} {s}"));
        // non-interactive: no tty needed
        args.retain(|a| a != "-it");
    }
    let st = Command::new("podman").args(&args).status().context("podman exec psql")?;
    Ok(st.code().unwrap_or(1))
}

pub fn server_version(cfg: &PgConfig) -> Option<String> {
    let out = podman(&[
        "exec",
        &container(),
        "psql",
        "-U",
        &cfg.user,
        "-d",
        &cfg.dbname,
        "-tAc",
        "select version()",
    ])
    .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    s.split(" on ").next().map(|x| x.trim().to_string())
}

pub fn connection_url(cfg: &PgConfig) -> String {
    format!(
        "postgres://{}:{}@{}:{}/{}",
        cfg.user,
        password(cfg),
        cfg.host,
        cfg.port,
        cfg.dbname
    )
}
