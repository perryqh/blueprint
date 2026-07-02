use crate::daemon::{self, LockInfo};
use crate::review_file;
use crate::selector::TextQuoteSelector;
use crate::store::Comment;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// HTTP client preconfigured with the daemon's CLI bearer token.
/// Daemon writes the token to ~/.blueprint/cli-token on startup; we read it here.
/// If the file is missing or empty, the client sends no auth header (legacy mode).
/// Also sets `X-Client-Cwd` to the current working directory so the daemon can
/// surface which repo published a blueprint in `blueprint status`.
pub(crate) fn cli_http_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = read_cli_token()
        && let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(reqwest::header::AUTHORIZATION, v);
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Some(s) = cwd.to_str()
        && let Ok(v) = reqwest::header::HeaderValue::from_str(s)
    {
        headers.insert("x-client-cwd", v);
    }
    reqwest::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(120))
        .build()
        .expect("reqwest client builds")
}

fn read_cli_token() -> Option<String> {
    let path = dirs::home_dir()?.join(".blueprint").join("cli-token");
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim().to_string();
    if t.is_empty() { None } else { Some(t) }
}

/// Drain a non-success `reqwest::Response` into an `anyhow::Error` with the
/// status code AND the response body, so the CLI surfaces a useful error
/// instead of just "400 Bad Request". Returns `Ok(resp)` on success so calls
/// can chain through to `.json()` etc.
async fn ensure_success(resp: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    bail!("{what} failed: {status} {body}")
}

#[derive(Parser, Debug)]
#[command(
    name = "blueprint",
    version,
    about = "Share interactive HTML blueprints with inline anchored comments"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Cmd,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Run the HTTP daemon in this process. Normally spawned automatically.
    Serve {
        /// Bind to a specific port (default: random).
        #[arg(long)]
        port: Option<u16>,
    },
    /// Print daemon status (port, PID, active blueprints).
    Status,
    /// Upload an HTML blueprint, print its review URL.
    Publish {
        /// Path to the HTML file to share.
        file: PathBuf,
        /// Custom slug (default: random adjective-month-animal).
        #[arg(long)]
        slug: Option<String>,
        /// If a slug already exists, replace its HTML in place.
        #[arg(long)]
        update: bool,
        /// Don't open the browser.
        #[arg(long)]
        no_open: bool,
        /// Emit machine-readable JSON ({slug, url, daemon}) on stdout instead of the human lines.
        /// Implies --no-open.
        #[arg(long)]
        json: bool,
    },
    /// Block until a reviewer clicks Finish Review, or stream new comments.
    Watch {
        slug: String,
        /// Stream each new comment as a line of JSON. Loops until daemon stops or Ctrl+C.
        #[arg(long)]
        stream: bool,
    },
    /// Fetch comments for a blueprint as a crit-compatible review.json.
    Fetch {
        slug: String,
        /// Output directory (default: ./.blueprint/<slug>/).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Add a comment as the given author.
    Comment {
        slug: String,
        /// Comment body.
        body: String,
        /// Text to anchor on (must appear in the published HTML).
        #[arg(long)]
        quote: Option<String>,
        /// Reply to an existing comment by ID.
        #[arg(long, value_name = "ID")]
        reply_to: Option<String>,
        /// Author name attached to the comment. Defaults to $USER, or "anonymous" if unset.
        #[arg(long)]
        author: Option<String>,
    },
    /// Remove a blueprint and its comments. If no blueprints remain, the daemon stops.
    Unpublish { slug: String },
    /// Mark a Submit-all batch as actively being worked on by the agent.
    /// Lights the slug-level "Claude is working on N comments" pill in the
    /// sidebar. Auto-clears when every parent_id receives a reply.
    #[command(subcommand)]
    BatchProcessing(BatchProcessingCmd),
    /// Run a stdio MCP server so any MCP-capable agent can drive blueprint
    /// (publish / update / wait-for-comments / reply / list) without the
    /// `/blueprint` skill. Speaks JSON-RPC 2.0 over stdin/stdout; wraps the
    /// same daemon HTTP API the CLI uses, auto-spawning the daemon on publish.
    Mcp,
}

#[derive(Subcommand, Debug)]
pub enum BatchProcessingCmd {
    /// Start the indicator. Pass every comment ID the agent is about to work
    /// through as a `--parent` flag (repeatable). The set is server-tracked
    /// so the indicator auto-clears when the last reply lands.
    Start {
        slug: String,
        /// Author shown in the indicator pill. Defaults to "Claude Code".
        #[arg(long, default_value = "Claude Code")]
        author: String,
        /// Parent comment ID. Pass once per comment in the batch.
        #[arg(long = "parent", value_name = "ID", required = true)]
        parents: Vec<String>,
    },
    /// Stop the indicator explicitly. Belt-and-braces — the server already
    /// auto-clears after the last reply. Use on early-exit paths (no edit,
    /// no replies) to avoid waiting for the 5-min TTL.
    End { slug: String },
}

pub async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Cmd::Serve { port } => daemon::run_foreground(port).await,
        Cmd::Status => status().await,
        Cmd::Publish {
            file,
            slug,
            update,
            no_open,
            json,
        } => publish(file, slug, update, no_open, json).await,
        Cmd::Watch { slug, stream } => watch(slug, stream).await,
        Cmd::Fetch { slug, output } => fetch(slug, output).await,
        Cmd::Comment {
            slug,
            body,
            quote,
            reply_to,
            author,
        } => comment(slug, body, quote, reply_to, author).await,
        Cmd::Unpublish { slug } => unpublish(slug).await,
        Cmd::BatchProcessing(BatchProcessingCmd::Start {
            slug,
            author,
            parents,
        }) => batch_processing_start(slug, author, parents).await,
        Cmd::BatchProcessing(BatchProcessingCmd::End { slug }) => batch_processing_end(slug).await,
        Cmd::Mcp => crate::mcp::run().await,
    }
}

async fn batch_processing_start(slug: String, author: String, parents: Vec<String>) -> Result<()> {
    let info = require_running()?;
    let client = cli_http_client();
    #[derive(Serialize)]
    struct Body<'a> {
        author: &'a str,
        parent_ids: &'a [String],
    }
    let resp = client
        .post(format!(
            "{}/api/blueprints/{}/batch-processing",
            base_url(&info),
            slug
        ))
        .json(&Body {
            author: &author,
            parent_ids: &parents,
        })
        .send()
        .await?;
    ensure_success(resp, "batch-processing start").await?;
    println!(
        "started batch-processing for {slug} on {} comment{}",
        parents.len(),
        if parents.len() == 1 { "" } else { "s" }
    );
    Ok(())
}

async fn batch_processing_end(slug: String) -> Result<()> {
    let info = require_running()?;
    let resp = cli_http_client()
        .delete(format!(
            "{}/api/blueprints/{}/batch-processing",
            base_url(&info),
            slug
        ))
        .send()
        .await?;
    ensure_success(resp, "batch-processing end").await?;
    println!("stopped batch-processing for {slug}");
    Ok(())
}

async fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("could not resolve current_exe")
}

pub(crate) async fn ensure_daemon() -> Result<LockInfo> {
    let exe = current_exe().await?;
    daemon::ensure_running(&exe).await
}

pub(crate) fn base_url(info: &LockInfo) -> String {
    format!("http://127.0.0.1:{}", info.port)
}

async fn status() -> Result<()> {
    match daemon::discover_running() {
        Some(info) => {
            println!("daemon: running on {}", base_url(&info));
            println!("  pid: {}", info.pid);
            let blueprints = list_blueprints(&info).await.unwrap_or_default();
            if blueprints.is_empty() {
                println!("\nno blueprints published.");
                return Ok(());
            }

            // Surface multi-repo sharing: if blueprints were published from
            // more than one cwd, note that the daemon is shared.
            let my_cwd = std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(str::to_string));
            let mut other_cwds: Vec<String> = blueprints
                .iter()
                .filter_map(|p| p.client_cwd.clone())
                .filter(|c| my_cwd.as_deref() != Some(c.as_str()))
                .collect();
            other_cwds.sort();
            other_cwds.dedup();
            if !other_cwds.is_empty() {
                let label = if other_cwds.len() == 1 {
                    "repo"
                } else {
                    "repos"
                };
                println!(
                    "  shared with {} other {}: {}",
                    other_cwds.len(),
                    label,
                    other_cwds.join(", ")
                );
            }

            println!("\nblueprints ({}):", blueprints.len());
            for p in blueprints {
                let activity = time_ago(p.last_activity_at);
                let comments = match (p.comment_count, p.unresolved_count) {
                    (0, _) => "no comments".to_string(),
                    (n, 0) => format!("{n} comment{}", if n == 1 { "" } else { "s" }),
                    (n, u) => format!(
                        "{n} comment{}  {u} unresolved",
                        if n == 1 { "" } else { "s" }
                    ),
                };
                let cwd_tag = match (&p.client_cwd, my_cwd.as_deref()) {
                    (Some(c), Some(me)) if c.as_str() != me => format!("  [from {c}]"),
                    _ => String::new(),
                };
                println!(
                    "  - {:<30} {}  (last activity {}){}",
                    p.slug, comments, activity, cwd_tag
                );
            }
        }
        None => println!("daemon: not running"),
    }
    Ok(())
}

#[derive(Deserialize)]
struct BlueprintRow {
    slug: String,
    #[serde(default)]
    comment_count: u32,
    #[serde(default)]
    unresolved_count: u32,
    #[serde(default)]
    last_activity_at: i64,
    #[serde(default)]
    client_cwd: Option<String>,
}

async fn list_blueprints(info: &LockInfo) -> Result<Vec<BlueprintRow>> {
    let client = cli_http_client();
    let r = client
        .get(format!("{}/api/blueprints", base_url(info)))
        .send()
        .await?
        .error_for_status()?
        .json::<Vec<BlueprintRow>>()
        .await?;
    Ok(r)
}

fn time_ago(ms: i64) -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let diff = (now - ms).max(0);
    let sec = diff / 1000;
    if sec < 60 {
        return format!("{sec}s ago");
    }
    let min = sec / 60;
    if min < 60 {
        return format!("{min}m ago");
    }
    let hr = min / 60;
    if hr < 24 {
        return format!("{hr}h ago");
    }
    let days = hr / 24;
    format!("{days}d ago")
}

#[derive(Serialize)]
struct CreateBlueprintBody<'a> {
    html: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    slug: Option<&'a str>,
}

#[derive(Deserialize)]
struct CreateBlueprintResp {
    slug: String,
}

#[derive(Serialize)]
struct UpdateBlueprintBody<'a> {
    html: &'a str,
}

async fn publish(
    file: PathBuf,
    slug: Option<String>,
    update: bool,
    no_open: bool,
    json: bool,
) -> Result<()> {
    let html =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let info = ensure_daemon().await?;
    let client = cli_http_client();

    let final_slug = if update {
        let target = slug
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--update requires --slug"))?;
        let body = UpdateBlueprintBody { html: &html };
        let resp = client
            .put(format!("{}/api/blueprints/{}", base_url(&info), target))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!(
                "update failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        target
    } else {
        let body = CreateBlueprintBody {
            html: &html,
            slug: slug.as_deref(),
        };
        let resp = client
            .post(format!("{}/api/blueprints", base_url(&info)))
            .json(&body)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!(
                "publish failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        let out = resp.json::<CreateBlueprintResp>().await?;
        out.slug
    };

    let url = format!("{}/b/{}", base_url(&info), final_slug);

    if json {
        let out = serde_json::json!({
            "slug": final_slug,
            "url": url,
            "daemon": base_url(&info),
            "updated": update,
        });
        println!("{out}");
        return Ok(());
    }

    println!("blueprint daemon at {}", base_url(&info));
    println!("Published as {}", url);

    if !no_open {
        let _ = open_browser(&url);
        println!("(opening in default browser)");
    }
    Ok(())
}

fn open_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
        return Ok(());
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    std::process::Command::new(cmd).arg(url).spawn().ok();
    Ok(())
}

async fn watch(slug: String, stream: bool) -> Result<()> {
    if stream {
        return watch_stream(slug).await;
    }
    let info = require_running()?;
    let url = format!("{}/api/blueprints/{}/wait", base_url(&info), slug);
    println!("Waiting for \"Finish Review\" on {}…", slug);
    let resp = cli_http_client()
        .get(&url)
        .timeout(Duration::from_secs(60 * 60 * 4))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("watch failed: {}", resp.status());
    }
    println!("Review complete.");
    Ok(())
}

/// Loop: long-poll /wait-comment, print each new comment as a JSON line.
/// Starts from "now" so only NEW comments stream, not the backlog.
async fn watch_stream(slug: String) -> Result<()> {
    let mut info = require_running()?;
    let client = cli_http_client();
    let mut since: i64 = crate::store::now_ms();
    eprintln!("Streaming new comments on {} (Ctrl+C to stop)…", slug);

    loop {
        let url = format!(
            "{}/api/blueprints/{}/wait-comment?since={}",
            base_url(&info),
            slug,
            since
        );
        let resp = match client
            .get(&url)
            .timeout(Duration::from_secs(60))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("(stream connection error: {e}; retrying in 2s)");
                tokio::time::sleep(Duration::from_secs(2)).await;
                // The daemon may have restarted on a different port. Re-read the lock file
                // so the next iteration points at whatever daemon is currently alive.
                if let Some(fresh) = daemon::discover_running() {
                    if fresh.port != info.port {
                        eprintln!("(daemon moved to port {}; reconnecting)", fresh.port);
                    }
                    info = fresh;
                }
                continue;
            }
        };
        if resp.status().as_u16() == 404 {
            eprintln!("blueprint `{slug}` was deleted; ending stream");
            return Ok(());
        }
        if !resp.status().is_success() {
            eprintln!("(stream error: {}; retrying in 2s)", resp.status());
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        #[derive(serde::Deserialize)]
        struct StreamResp {
            comments: Vec<Comment>,
            server_ts: i64,
        }
        let body: StreamResp = resp.json().await?;
        since = body.server_ts;
        for c in body.comments {
            // One JSON object per line. Stable schema = same as /api/blueprints/:slug/comments.
            // Flush after each line: Rust's stdout is block-buffered when piped, so without
            // an explicit flush the line sits in the pipe buffer (~4KB) before downstream
            // consumers see it — which makes the "event-driven" stream not event-driven at all.
            if let Ok(s) = serde_json::to_string(&c) {
                use std::io::Write;
                println!("{s}");
                let _ = std::io::stdout().flush();
            }
        }
    }
}

async fn fetch(slug: String, output: Option<PathBuf>) -> Result<()> {
    let info = require_running()?;
    let url = format!("{}/api/blueprints/{}/comments", base_url(&info), slug);
    let resp = cli_http_client()
        .get(&url)
        .send()
        .await?
        .error_for_status()?;
    #[derive(Deserialize)]
    struct CR {
        comments: Vec<Comment>,
    }
    let cr: CR = resp.json().await?;
    let review = review_file::build(&slug, cr.comments);

    let dir = output.unwrap_or_else(|| Path::new(".blueprint").join(&slug));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("review.json");
    let bytes = serde_json::to_vec_pretty(&review)?;
    std::fs::write(&path, &bytes)?;
    println!("Wrote {}", path.display());
    Ok(())
}

async fn comment(
    slug: String,
    body: String,
    quote: Option<String>,
    reply_to: Option<String>,
    author: Option<String>,
) -> Result<()> {
    let info = require_running()?;
    let client = cli_http_client();
    let author =
        author.unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "anonymous".into()));

    if let Some(parent_id) = reply_to {
        #[derive(Serialize)]
        struct R<'a> {
            author: &'a str,
            body: &'a str,
        }
        let resp = client
            .post(format!(
                "{}/api/blueprints/{}/comments/{}/replies",
                base_url(&info),
                slug,
                parent_id
            ))
            .json(&R {
                author: &author,
                body: &body,
            })
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!(
                "reply failed: {} {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }
        let c: Comment = resp.json().await?;
        println!("Added reply {} to {}", c.id, parent_id);
        return Ok(());
    }

    let q = quote
        .ok_or_else(|| anyhow::anyhow!("--quote required for new comments (or use --reply-to)"))?;

    // Fetch the rendered HTML to extract prefix/suffix context.
    let raw = client
        .get(format!("{}/api/blueprints/{}/raw", base_url(&info), slug))
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;
    let stripped = strip_tags(&raw);
    if !stripped.contains(&q) {
        bail!(
            "quote {:?} was not found in the rendered text of blueprint `{}`.\n  \
             The quote must appear literally in the visible HTML (case and whitespace matter).\n  \
             tip: open {}/b/{} in a browser, copy the exact text, and retry.",
            q,
            slug,
            base_url(&info),
            slug
        );
    }
    let (prefix, suffix) = context_around(&raw, &q);
    let selector = TextQuoteSelector {
        ty: "TextQuoteSelector".into(),
        exact: q,
        prefix,
        suffix,
    };

    #[derive(Serialize)]
    struct C<'a> {
        author: &'a str,
        body: &'a str,
        selector: &'a TextQuoteSelector,
    }
    let resp = client
        .post(format!(
            "{}/api/blueprints/{}/comments",
            base_url(&info),
            slug
        ))
        .json(&C {
            author: &author,
            body: &body,
            selector: &selector,
        })
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!(
            "comment failed: {} {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );
    }
    let c: Comment = resp.json().await?;
    println!("Added comment {}", c.id);
    Ok(())
}

/// Strip tags naively, then find the quote, returning ~32 chars of surrounding context.
fn context_around(html: &str, quote: &str) -> (Option<String>, Option<String>) {
    let text = strip_tags(html);
    if let Some(idx) = text.find(quote) {
        let pre_start = idx.saturating_sub(32);
        let suf_end = (idx + quote.len() + 32).min(text.len());
        let prefix = Some(text[pre_start..idx].to_string());
        let suffix = Some(text[idx + quote.len()..suf_end].to_string());
        (prefix, suffix)
    } else {
        (None, None)
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

async fn unpublish(slug: String) -> Result<()> {
    let info = require_running()?;
    let client = cli_http_client();
    let resp = client
        .delete(format!("{}/api/blueprints/{}", base_url(&info), slug))
        .send()
        .await?;
    if !resp.status().is_success() {
        bail!("unpublish failed: {}", resp.status());
    }
    println!("Removed blueprint {}", slug);

    // Ask the daemon to stop *if* no blueprints remain. The check happens server-side
    // under the store mutex, so a concurrent publish from another repo either
    // commits before this read (count > 0 → daemon stays up) or after it
    // (graceful shutdown still serves the publish, then exits; next CLI call
    // respawns the daemon). Either way, no blueprint is lost. See `shutdown_if_empty`
    // in src/server.rs.
    let resp = client
        .post(format!("{}/api/shutdown-if-empty", base_url(&info)))
        .send()
        .await;
    if let Ok(r) = resp
        && r.status() == reqwest::StatusCode::NO_CONTENT
    {
        println!("(no blueprints remain, stopping daemon)");
    }
    Ok(())
}

pub(crate) fn require_running() -> Result<LockInfo> {
    daemon::discover_running().ok_or_else(|| {
        anyhow::anyhow!("blueprint daemon is not running (try `blueprint publish` first)")
    })
}
