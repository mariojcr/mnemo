// SPDX-FileCopyrightText: 2026 Mario Campos <https://github.com/mariojcr>
// SPDX-License-Identifier: Apache-2.0

//! mnemo: long-term memory for agents, models and people (subcommands: docs/architecture.md).

#![allow(clippy::collapsible_if)]

mod admin;
mod api;
mod app;
mod auth;
mod client;
mod distill;
mod episode;
mod facts;
mod galaxy;
mod globe;
mod hook;
mod judge;
mod llm;
mod login;
mod mcp;
mod meter;
mod model;
mod oidc;
mod onnx;
mod ops;
mod query;
mod records;
mod render;
mod storage;
mod text;
mod update;
mod value;

use app::{App, Cfg};
use judge::Judge;
use llm::Llm;
use log::{error, info, warn};
use std::sync::Arc;
use std::time::Duration;
use storage::{Db, RawStore};

fn env(name: &str) -> String {
    std::env::var(name)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn env_or(name: &str, default: &str) -> String {
    let v = env(name);
    if v.is_empty() { default.to_string() } else { v }
}

async fn shutdown_signal() {
    let mut term =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("SIGTERM");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

async fn open_storage() -> Result<(Arc<storage::ch::ClickHouse>, String), String> {
    let url = env("MNEMO_CH_URL");
    if url.is_empty() {
        return Err(
            "mnemo needs ClickHouse: set MNEMO_CH_URL (and MNEMO_CH_USER, MNEMO_CH_PASSWORD, MNEMO_CH_DB)"
                .into(),
        );
    }
    let ch = storage::ch::ClickHouse::new(
        &url,
        &env_or("MNEMO_CH_DB", "mnemo"),
        &env_or("MNEMO_CH_USER", "mnemo"),
        &env("MNEMO_CH_PASSWORD"),
    )?
    // The operator creates the Replicated database on every replica; otherwise a missing one would
    // be created as a plain database on whichever replica answers.
    .require_replicated(env("MNEMO_CH_REPLICATED") == "1");
    ch.wait_ready(env_or("MNEMO_CH_WAIT_SECONDS", "30").parse().unwrap_or(30))
        .await?;
    Db::migrate(&ch).await?;
    Ok((Arc::new(ch), format!("clickhouse {url}")))
}

/// `model=input,cache_read,cache_write,output;…` in dollars per million tokens.
fn parse_prices(s: &str) -> std::collections::HashMap<String, [f64; 4]> {
    s.split(';')
        .filter_map(|item| {
            let (model, nums) = item.split_once('=')?;
            let v: Vec<f64> = nums
                .split(',')
                .filter_map(|x| x.trim().parse().ok())
                .collect();
            (v.len() == 4).then(|| (model.trim().to_string(), [v[0], v[1], v[2], v[3]]))
        })
        .collect()
}

fn llm_from(
    url_var: &str,
    key_var: &str,
    fallback: Option<(&str, &str)>,
) -> Result<Option<Llm>, String> {
    let url = env(url_var);
    let (url, key) = match (url.is_empty(), fallback) {
        (false, _) => (url, env(key_var)),
        (true, Some((u, k))) if !env(u).is_empty() => (env(u), env(k)),
        _ => return Ok(None),
    };
    Llm::new(&url, &key).map(Some)
}

async fn build_app(
    ch: Arc<storage::ch::ClickHouse>,
    where_: String,
    writer: Option<String>,
) -> Result<Arc<App>, String> {
    let replicated = ch.is_replicated();
    let db: Arc<dyn Db> = ch.clone();
    let raw: Option<Arc<dyn RawStore>> = Some(ch);
    let llm = llm_from("MNEMO_LLM_URL", "MNEMO_LLM_KEY", None)?.map(|l| {
        l.with_json(env("MNEMO_LLM_JSON") == "1")
            .without_thinking(env("MNEMO_LLM_THINKING") == "off")
    });
    let embedder = (env("MNEMO_EMBED") != "off").then(|| {
        crate::app::Embedder::Local(crate::onnx::Local::start(
            std::path::PathBuf::from(env_or(
                "MNEMO_EMBED_DIR",
                &format!("{}/models/embeddinggemma-2", env_or("MNEMO_HOME", ".")),
            )),
            env_or("ORT_DYLIB_PATH", "libonnxruntime.so"),
            env_or("MNEMO_EMBED_THREADS", "4").parse().unwrap_or(4),
            // 2,048 tokens: ~0.5 GB extra peak memory and ~2-4 s on CPU; memories stay under 200
            env_or("MNEMO_EMBED_MAX_TOKENS", "2048")
                .parse()
                .unwrap_or(2048),
        ))
    });
    let embed_model = if embedder.is_some() {
        crate::onnx::MODEL_NAME.to_string()
    } else {
        String::new()
    };
    let extract_model = env("MNEMO_EXTRACT_MODEL");
    let decide = llm_from(
        "MNEMO_DECIDE_URL",
        "MNEMO_DECIDE_KEY",
        Some(("MNEMO_LLM_URL", "MNEMO_LLM_KEY")),
    )?;
    let decide_model = env("MNEMO_DECIDE_MODEL");
    // A decisions verdict (6-7 questions, ~22k tokens) takes 20-46 s on an idle vLLM; at 60 s half
    // timed out under load, and a timeout silently degrades to a heuristic verdict.
    let judge_timeout = Duration::from_secs(
        env_or("MNEMO_JUDGE_TIMEOUT", "180")
            .parse()
            .ok()
            .filter(|s| *s > 0)
            .unwrap_or(180),
    );
    let judge = match (env_or("MNEMO_JUDGE", "auto").as_str(), decide, llm.clone()) {
        ("heuristic", _, _) => Judge::Heuristic,
        ("auto" | "decisions", Some(d), _) if !decide_model.is_empty() => {
            Judge::Decisions(d, decide_model.clone(), judge_timeout)
        }
        ("auto" | "llm", _, Some(l)) if !extract_model.is_empty() => {
            Judge::Llm(l, extract_model.clone(), judge_timeout)
        }
        _ => Judge::Heuristic,
    };
    let judge_name = match &judge {
        Judge::Decisions(..) => "decisions",
        Judge::Llm(..) => "llm",
        Judge::Heuristic => "heuristic",
    };
    let oidc = oidc::Oidc::from_env(env)?;
    // An unknown value fails startup instead of falling back to the less secure mode.
    let sso_only = match env("MNEMO_AUTH").as_str() {
        "" | "keys" => false,
        "sso" => true,
        other => return Err(format!("MNEMO_AUTH={other}: valid values are keys or sso")),
    };
    if sso_only && oidc.is_none() {
        return Err("MNEMO_AUTH=sso without MNEMO_OIDC_ISSUER: nobody could sign in".into());
    }
    if let (true, Some(o)) = (sso_only, &oidc) {
        if o.cfg.subject_claim == "email" {
            return Err("MNEMO_AUTH=sso requires MNEMO_OIDC_SUBJECT_CLAIM=sub (or another immutable claim): people can change their own email".into());
        }
        if o.cfg.allowed_groups.is_empty() {
            return Err("MNEMO_AUTH=sso requires MNEMO_OIDC_ALLOWED_GROUPS: otherwise any account at the provider gets in".into());
        }
    }
    // A static key exposed to the internet is exactly what SSO-only mode avoids.
    let admin_key = if sso_only {
        if !env("MNEMO_ADMIN_KEY").is_empty() {
            warn!("MNEMO_AUTH=sso: ignoring MNEMO_ADMIN_KEY");
        }
        String::new()
    } else {
        env("MNEMO_ADMIN_KEY")
    };
    let has_keys = !db.list_keys().await?.is_empty();
    // There is no open mode, not even locally.
    if admin_key.is_empty() && !has_keys && oidc.is_none() {
        return Err(
            "no way to sign in: set MNEMO_ADMIN_KEY, create a key with `mnemo key create --subject <you>` or configure MNEMO_OIDC_*"
                .into(),
        );
    }
    let cfg = Cfg {
        admin_key,
        sso_only,
        extract_model,
        llm_context: env_or("MNEMO_LLM_CONTEXT", "16384")
            .parse()
            .unwrap_or(16384),
        extract_budget_tokens: env_or("MNEMO_EXTRACT_BUDGET_TOKENS", "6000")
            .parse()
            .unwrap_or(6000),
        llm_timeout_s: env_or("MNEMO_LLM_TIMEOUT", "600").parse().unwrap_or(600),
        distill_parallel: env_or("MNEMO_DISTILL_PARALLEL", "4").parse().unwrap_or(4),
        embed_model,
        settle_ms: env_or("MNEMO_SETTLE_SECONDS", "120")
            .parse::<i64>()
            .unwrap_or(120)
            * 1000,
        session_budget_tokens: env_or("MNEMO_SESSION_BUDGET_TOKENS", "4000")
            .parse()
            .unwrap_or(4000),
        mute_after: env_or("MNEMO_MUTE_AFTER", "6").parse().unwrap_or(6),
        eval_sample: env_or("MNEMO_EVAL_SAMPLE", "0.25")
            .parse::<f64>()
            .unwrap_or(0.25)
            .clamp(0.0, 1.0),
        prices: parse_prices(&env("MNEMO_PRICES")),
    };
    info!(
        "mnemo {}: {where_} · {} · extract={:?} · embed={:?} · judge={judge_name} · {}",
        env!("CARGO_PKG_VERSION"),
        match &writer {
            Some(w) => format!("replica of {w}"),
            None => "writer".into(),
        },
        cfg.extract_model,
        cfg.embed_model,
        if sso_only {
            "SSO only (session or the provider's OAuth token)"
        } else {
            "with keys"
        }
    );
    meter::install(db.clone());
    let info = serde_json::json!({
        "role": match &writer { Some(w) => format!("replica of {w}"), None => "writer".into() },
        "clickhouse": {"url": env("MNEMO_CH_URL"), "db": env_or("MNEMO_CH_DB", "mnemo"), "replicated": replicated},
        "llm": {"url": env("MNEMO_LLM_URL"), "extract_model": cfg.extract_model},
        "embed": {"model": cfg.embed_model, "runtime": "onnx"},
        "judge": judge_name,
        "judge_timeout_s": judge_timeout.as_secs(),
        "settle_seconds": cfg.settle_ms / 1000,
        "llm_context": cfg.llm_context,
        "extract_budget_tokens": cfg.extract_budget_tokens,
        "llm_timeout_s": cfg.llm_timeout_s,
        "distill_parallel": cfg.distill_parallel,
        "session_budget_tokens": cfg.session_budget_tokens,
        "mute_after": cfg.mute_after,
        "eval_sample": cfg.eval_sample,
        "priced_models": cfg.prices.keys().collect::<Vec<_>>(),
        "auth": {
            "admin_key": !cfg.admin_key.is_empty(),
            "oidc": oidc.as_ref().map(|o| serde_json::json!({"issuer": o.cfg.issuer, "client_id": o.cfg.client_id, "admin_emails": o.cfg.admin_emails.len(), "admin_groups": o.cfg.admin_groups, "session_hours": o.cfg.session_hours})),
        },
    });
    let mut app = App::new(db, raw, llm, embedder, judge, cfg);
    app.writer = writer;
    app.oidc = oidc;
    app.info = info;
    app.forwarded_hops = env_or("MNEMO_FORWARDED_HOPS", "1").parse().unwrap_or(1);
    let app = Arc::new(app);
    let n = app.refresh().await?;
    let (e, m) = app.globe.read().unwrap().counts();
    info!("globe loaded: {e} entities, {m} memories ({n} rows)");
    // The writer skips unchanged log heads; once a minute it refreshes anyway, for external edits.
    let bg = app.clone();
    tokio::spawn(async move {
        let mut tick = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            tick = (tick + 1) % 60;
            let r = if tick == 0 {
                bg.refresh().await.map(|_| ())
            } else {
                bg.refresh_if_changed().await.map(|_| ())
            };
            if let Err(e) = r {
                error!("refresh: {e}");
            }
        }
    });
    let bg = app.clone();
    tokio::spawn(async move {
        loop {
            match bg.refresh_usefulness().await {
                Ok(n) if n > 0 => info!("{n} memories muted for never being useful"),
                Ok(_) => {}
                Err(e) => warn!("usefulness: {e}"),
            }
            tokio::time::sleep(Duration::from_secs(300)).await;
        }
    });
    Ok(app)
}

async fn keys_cmd(args: &[String]) -> Result<(), String> {
    let (ch, _) = open_storage().await?;
    let db: Arc<dyn Db> = ch;
    let flag = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    match args.first().map(|s| s.as_str()) {
        Some("create") => {
            let kind = auth::Kind::parse(&flag("--kind").unwrap_or_else(|| "user".into())).ok_or("--kind admin|service|user")?;
            let subject = flag("--subject").ok_or("missing --subject")?;
            let app = App::new(db, None, None, None, Judge::Heuristic, Cfg::default());
            let expires = flag("--days").and_then(|d| d.parse::<i64>().ok()).filter(|d| *d > 0).map(|d| app::now_ms() + d * 86_400_000).unwrap_or(0);
            let (row, key) = app.create_key(&flag("--name").unwrap_or_else(|| subject.clone()), kind, &subject, expires, "cli").await?;
            println!("{key}");
            eprintln!("key {} for {} ({}). Save it: it will not be shown again.", row.id, row.subject, row.kind);
            Ok(())
        }
        Some("list") => {
            for k in db.list_keys().await? {
                println!("{}\t{}\t{}\t{}{}", k.id, k.kind, k.subject, k.name, if k.revoked { "\t(revoked)" } else { "" });
            }
            Ok(())
        }
        Some("revoke") => {
            let id = args.get(1).ok_or("usage: mnemo key revoke <id>")?;
            if db.revoke_key(id).await? { Ok(()) } else { Err(format!("no active key {id}")) }
        }
        _ => Err("usage: mnemo key create --subject <s> [--kind user|service|admin] [--name <n>] [--days <n>] | list | revoke <id>".into()),
    }
}

async fn serve(replica: bool) -> Result<(), String> {
    let listen = env_or("MNEMO_LISTEN", client::DEFAULT_LISTEN);
    let writer = if replica {
        let w = env("MNEMO_WRITER_URL");
        if w.is_empty() {
            return Err("`mnemo api` needs MNEMO_WRITER_URL (the URL of the `mnemo serve`)".into());
        }
        Some(w.trim_end_matches('/').to_string())
    } else {
        None
    };
    let (ch, where_) = open_storage().await?;
    let instance = format!(
        "{}-{}",
        env_or("HOSTNAME", "local"),
        uuid::Uuid::new_v4().simple()
    );
    if !replica {
        claim_writer(&ch, &instance).await?;
        let hb = ch.clone();
        let me = instance.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(10)).await;
                match hb.heartbeat(&me, app::now_ms()).await {
                    Ok(true) => {}
                    // Writing on would hand out the same sequence twice; the supervisor restarts us
                    // and we wait our turn.
                    Ok(false) => {
                        error!("another writer has taken over the lease on this database: exiting");
                        std::process::exit(3);
                    }
                    Err(e) => warn!("writer lease: {e}"),
                }
            }
        });
    }
    let app = build_app(ch.clone(), where_, writer).await?;
    let t = app.clone();
    tokio::spawn(async move { t.things_loop().await });
    if !replica {
        let d = app.clone();
        tokio::spawn(async move { d.distill_loop().await });
        let e = app.clone();
        tokio::spawn(async move { e.merge_entities_loop().await });
        let b = app.clone();
        tokio::spawn(async move { b.backfill_embeddings().await });
    }
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("{listen}: {e}"))?;
    info!("listening on http://{listen}");
    axum::serve(
        listener,
        api::router(app).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .ok();
    if !replica {
        ch.release_writer(&instance, app::now_ms()).await.ok();
    }
    Ok(())
}

async fn claim_writer(ch: &storage::ch::ClickHouse, instance: &str) -> Result<(), String> {
    let mut last = String::new();
    for i in 0..storage::ch::WRITER_LEASE_MS / 1000 + 6 {
        match ch.claim_writer(instance, app::now_ms()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                if i == 0 {
                    warn!("{e}; waiting for it to expire");
                }
                last = e;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Err(last)
}

/// `~/.mnemo/config.env` exists because processes Claude Code starts inherit its environment, not
/// the shell's. The environment wins.
fn load_config(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("export ").unwrap_or(l).split_once('='))
        .map(|(k, v)| {
            (
                k.trim().to_string(),
                v.trim().trim_matches(['"', '\'']).to_string(),
            )
        })
        .filter(|(k, _)| {
            k.starts_with("MNEMO_")
                && k != "MNEMO_HOME"
                && std::env::var_os(k).is_none_or(|v| v.is_empty())
        })
        .collect()
}

fn version() -> String {
    match option_env!("MNEMO_COMMIT").map(str::trim) {
        Some(c) if !c.is_empty() => format!("{} ({c})", env!("CARGO_PKG_VERSION")),
        _ => env!("CARGO_PKG_VERSION").to_string(),
    }
}

fn main() {
    if let Ok(text) = std::fs::read_to_string(client::home().join("config.env")) {
        for (k, v) in load_config(&text) {
            // SAFETY: this is still the only thread; the tokio runtime is created right after.
            unsafe { std::env::set_var(k, v) };
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(run());
}

async fn run() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(|s| s.as_str()).unwrap_or("serve");
    // stdout belongs to the protocol in `mcp` and to Claude Code in `hook`: logs go to stderr.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(
        if matches!(mode, "mcp" | "hook") {
            "warn"
        } else {
            "info"
        },
    ))
    .target(env_logger::Target::Stderr)
    .init();
    match mode {
        "version" | "--version" | "-V" => println!("mnemo {}", version()),
        "update" => match update::run(&args[2..]).await {
            Ok(msg) => println!("{msg}"),
            Err(e) => {
                eprintln!("mnemo update: {e}");
                std::process::exit(1);
            }
        },
        "bench" => bench::run(
            args.get(2)
                .and_then(|s| s.parse().ok())
                .unwrap_or(200_000usize),
        ),
        "health" => {
            let addr =
                env_or("MNEMO_LISTEN", client::DEFAULT_LISTEN).replace("0.0.0.0", "127.0.0.1");
            let ok = reqwest::Client::new()
                .get(format!("http://{addr}/healthz"))
                .timeout(Duration::from_secs(3))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success());
            std::process::exit(if ok { 0 } else { 1 });
        }
        "mcp" => {
            let c = match client::Client::from_env(Duration::from_secs(60)) {
                Ok(c) => c,
                Err(e) => {
                    error!("mcp: {e}");
                    std::process::exit(1);
                }
            };
            if let Err(e) = c.ensure().await {
                warn!("{e}");
            }
            if let Err(e) = mcp::stdio_bridge(&c).await {
                error!("mcp: {e}");
                std::process::exit(1);
            }
        }
        "hook" => hook::run(&args[2..]).await,
        "login" => {
            let server = args.get(2).cloned().unwrap_or_else(|| env("MNEMO_URL"));
            let r = if server.trim().is_empty() {
                Err("usage: mnemo login <server url> (or MNEMO_URL)".into())
            } else {
                login::login(&server).await
            };
            if let Err(e) = r {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        "logout" => {
            if let Err(e) = login::logout() {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        "key" | "keys" => {
            if let Err(e) = keys_cmd(&args[2..]).await {
                eprintln!("{e}");
                std::process::exit(1);
            }
        }
        "serve" | "api" => {
            if let Err(e) = serve(mode == "api").await {
                error!("startup: {e}");
                std::process::exit(1);
            }
        }
        other => {
            eprintln!(
                "unknown mode: {other}\nusage: mnemo [serve|api|mcp|hook <event>|login|logout|key …|update [version] [--allow-downgrade]|bench [n]|health|version]"
            );
            std::process::exit(2);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn prices_parse_per_model() {
        let p = super::parse_prices("claude-sonnet=3,0.3,3.75,15; bad=1,2 ;x=1,1,1,1");
        assert_eq!(p.get("claude-sonnet"), Some(&[3.0, 0.3, 3.75, 15.0]));
        assert_eq!(p.len(), 2, "an incomplete entry is ignored");
    }

    #[test]
    fn config_file_only_fills_mnemo_vars() {
        let got = super::load_config(
            "# c\nMNEMO_X_TEST_A=1\nexport MNEMO_X_TEST_B=\"two\"\nPATH=/evil\nMNEMO_HOME=/x\n\n",
        );
        assert_eq!(
            got,
            vec![
                ("MNEMO_X_TEST_A".into(), "1".into()),
                ("MNEMO_X_TEST_B".into(), "two".into())
            ]
        );
    }
}

mod bench {
    //! Benchmarks the fast path (no model calls) on a synthetic globe.
    use crate::globe::{Globe, RecallOpts};
    use crate::model::{Entity, Link, Memory, Role};
    use std::time::Instant;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Zipf-like over a 20,000-word vocabulary, like real text.
    fn word(seed: &mut u64) -> String {
        let u = (rng(seed) % 1_000_000) as f64 / 1_000_000.0;
        let k = (20_000f64.powf(u) as u64).saturating_sub(1);
        format!("w{k}x")
    }

    pub fn run(n: usize) {
        let mut seed = 0x9E3779B97F4A7C15u64;
        let mut g = Globe::default();
        let t0 = Instant::now();
        let n_ent = 2000;
        for i in 0..n_ent {
            let parent = if i >= 100 {
                Some(format!("e{}", rng(&mut seed) % 100))
            } else {
                None
            };
            g.upsert_entity(
                Entity {
                    id: format!("e{i}"),
                    scope: "global".into(),
                    kind: "system".into(),
                    name: format!("svc{i}"),
                    aliases: vec![format!("service {i}")],
                    parent,
                    ..Default::default()
                },
                false,
                1,
            );
        }
        let scopes_pool = ["global", "team:a", "team:b", "user:u1", "user:u2"];
        for i in 0..n {
            let mut what = String::new();
            for _ in 0..14 {
                what.push_str(&word(&mut seed));
                what.push(' ');
            }
            let links = (0..3)
                .map(|k| Link {
                    entity: format!("e{}", rng(&mut seed) % n_ent),
                    role: Role::ALL[k % 3],
                    subrole: String::new(),
                })
                .collect();
            let t = 1_750_000_000_000 + (rng(&mut seed) % 10_000_000_000) as i64;
            g.upsert_memory(
                Memory {
                    id: format!("m{i}"),
                    scope: scopes_pool[(rng(&mut seed) % 5) as usize].into(),
                    kind: "fact".into(),
                    why: (i % 3 == 0).then(|| "because of the cache".to_string()),
                    what,
                    occurred_from: Some(t),
                    known_at: t,
                    valid_from: t,
                    valid_to: (i % 7 == 0).then_some(t + 1),
                    confidence: 0.8,
                    source_episodes: vec!["x".into()],
                    links,
                    ..Default::default()
                },
                false,
                1,
            );
        }
        let tl = Instant::now();
        g.finish_batch();
        let layout_ms = tl.elapsed().as_secs_f64() * 1000.0;
        let build = t0.elapsed();
        let scopes: Vec<String> = ["global", "team:a", "user:u1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let mut times = Vec::new();
        let mut hits = 0usize;
        for _ in 0..2000 {
            let q = format!(
                "¿por qué falló el {} de svc{} con {}?",
                word(&mut seed),
                rng(&mut seed) % n_ent,
                word(&mut seed)
            );
            let t = Instant::now();
            let out = g.recall(&RecallOpts {
                query: &q,
                scopes: &scopes,
                now: 1_760_000_000_000,
                as_of: None,
                limit: 8,
                w: None,
                entities: &[],
                query_vec: None,
                since: None,
                until: None,
            });
            times.push(t.elapsed().as_secs_f64() * 1000.0);
            hits += out.hits.len();
        }
        times.sort_by(|a, b| a.total_cmp(b));
        let p = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize];
        let ti = Instant::now();
        g.upsert_entity(
            Entity {
                id: "e-new".into(),
                scope: "global".into(),
                kind: "system".into(),
                name: "newsvc".into(),
                parent: Some("e1".into()),
                ..Default::default()
            },
            false,
            2,
        );
        for i in 0..20 {
            g.upsert_memory(
                Memory {
                    id: format!("new{i}"),
                    scope: "global".into(),
                    kind: "fact".into(),
                    what: "fresh".into(),
                    known_at: 1,
                    valid_from: 1,
                    confidence: 0.8,
                    source_episodes: vec!["x".into()],
                    links: vec![Link {
                        entity: "e-new".into(),
                        role: Role::About,
                        subrole: String::new(),
                    }],
                    ..Default::default()
                },
                false,
                3,
            );
        }
        g.finish_batch();
        let incr_ms = ti.elapsed().as_secs_f64() * 1000.0;
        let mut tiles = Vec::new();
        for z in [0u32, 3, 6, 10] {
            let mut ts = Vec::new();
            let mut size = 0usize;
            for k in 0..60 {
                let side = 1u32 << z;
                let (x, y) = if z == 0 {
                    (0, 0)
                } else {
                    (
                        (rng(&mut seed) % side as u64) as u32,
                        (rng(&mut seed) % side as u64) as u32,
                    )
                };
                let t = Instant::now();
                let v = crate::galaxy::tile(&g, z, x, y, &scopes, false);
                ts.push(t.elapsed().as_secs_f64() * 1000.0);
                if k == 0 {
                    size = v.to_string().len();
                }
            }
            ts.sort_by(|a, b| a.total_cmp(b));
            tiles.push(format!(
                "\"z{z}\": {{\"p50_ms\": {:.2}, \"p95_ms\": {:.2}, \"bytes\": {size}}}",
                ts[ts.len() / 2],
                ts[ts.len() * 95 / 100]
            ));
        }
        println!(
            "{{\"memories\": {n}, \"entities\": {n_ent}, \"build_s\": {:.2}, \"layout_ms\": {layout_ms:.0}, \"small_change_ms\": {incr_ms:.0}, \"p50_ms\": {:.2}, \"p95_ms\": {:.2}, \"p99_ms\": {:.2}, \"avg_hits\": {:.1}, \"galaxy\": {{{}}}}}",
            build.as_secs_f64(),
            p(0.5),
            p(0.95),
            p(0.99),
            hits as f64 / 2000.0,
            tiles.join(", ")
        );
    }
}
