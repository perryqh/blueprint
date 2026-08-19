use crate::daemon::{self, LockInfo};
use crate::review_file;
use crate::selector::TextQuoteSelector;
use crate::store::Comment;
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

/// HTTP client preconfigured with the daemon's CLI bearer token.
/// Daemon writes the token to ~/.blueprint/cli-token on startup; we read it here.
/// If the file is missing or empty, the client sends no auth header (legacy mode).
/// Also sets `X-Client-Cwd` to the current working directory so the daemon can
/// surface which repo published a blueprint in `blueprint status`.
/// Built once per process and cloned thereafter: `reqwest::Client` is
/// `Arc`-backed, so a clone shares the connection pool rather than rebuilding the
/// whole TLS/pool stack — and the headers it bakes in (bearer token, cwd) are
/// fixed for this process's lifetime anyway. A build failure degrades to a
/// header-less default client instead of panicking in library code; the daemon
/// then answers 401 and the user gets a message rather than a backtrace.
pub(crate) fn cli_http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_cli_http_client).clone()
}

fn build_cli_http_client() -> reqwest::Client {
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
        .unwrap_or_else(|e| {
            tracing::warn!(%e, "could not build the CLI http client; falling back to an unauthenticated default");
            reqwest::Client::new()
        })
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
pub(crate) async fn ensure_success(
    resp: reqwest::Response,
    what: &str,
) -> Result<reqwest::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    bail!("{what} failed: {status} {body}")
}

/// Reject port 0. `resolve_port` treats `None` as "use the default", so a 0 here
/// wouldn't mean "pick a free port" — it would bind an arbitrary ephemeral port
/// and silently break the OAuth callback, which needs a predictable one.
fn parse_port(s: &str) -> Result<u16, String> {
    match s.parse::<u16>() {
        Ok(0) => Err("port 0 is not allowed; omit --port to use the default (7321)".into()),
        Ok(p) => Ok(p),
        Err(e) => Err(e.to_string()),
    }
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
        /// Bind to a specific port. Defaults to $BLUEPRINT_PORT, else 7321 —
        /// the port baked into the registered GitHub OAuth callback URL, so
        /// overriding it breaks the login round-trip.
        #[arg(long, value_parser = parse_port)]
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
        /// Requires --slug: there is nothing to update without a target, and
        /// letting clap enforce that yields a usage message *before* we spawn a
        /// daemon and read the file.
        #[arg(long, requires = "slug")]
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

pub async fn run(cli: Cli, env: crate::auth::EnvFile) -> Result<()> {
    match cli.command {
        Cmd::Serve { port } => daemon::run_foreground(port, env).await,
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

fn current_exe() -> Result<PathBuf> {
    std::env::current_exe().context("could not resolve current_exe")
}

pub(crate) async fn ensure_daemon() -> Result<LockInfo> {
    let exe = current_exe()?;
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
        // clap guarantees --slug is present alongside --update, so this is
        // unreachable rather than a real runtime path.
        let target = slug
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--update requires --slug"))?;
        let body = UpdateBlueprintBody { html: &html };
        let resp = client
            .put(format!("{}/api/blueprints/{}", base_url(&info), target))
            .json(&body)
            .send()
            .await?;
        ensure_success(resp, "update").await?;
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
        let out = ensure_success(resp, "publish")
            .await?
            .json::<CreateBlueprintResp>()
            .await?;
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

    // Only claim to have opened a browser if the spawn actually worked —
    // printing it unconditionally sent people looking for a window that was
    // never going to appear (headless box, no xdg-open).
    if !no_open && open_browser(&url) {
        println!("(opening in default browser)");
    }
    Ok(())
}

/// Returns whether a browser was actually launched.
fn open_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(target_os = "linux")]
    let cmd = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = url;
        false
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    std::process::Command::new(cmd)
        .arg(url)
        .spawn()
        .inspect_err(|e| tracing::debug!(%e, "could not launch a browser"))
        .is_ok()
}

async fn watch(slug: String, stream: bool) -> Result<()> {
    if stream {
        return watch_stream(slug).await;
    }
    let info = require_running()?;
    // No "since" anchor: the daemon holds a pending-finish latch, so a click that
    // happened before this process even started is still claimed here. Anchoring
    // on start time would throw away exactly the case this is meant to survive.
    let url = format!("{}/api/blueprints/{}/wait", base_url(&info), slug);
    println!("Waiting for \"Finish Review\" on {}…", slug);

    #[derive(serde::Deserialize)]
    struct WaitResp {
        finished_at: Option<i64>,
    }

    // The daemon caps each `/wait` at an hour so an abandoned watch eventually
    // returns its long-poll slot. A capped poll answers `finished_at: null`,
    // which means "nothing yet" — reconnect. Only a real timestamp ends the
    // loop, so a reviewer who takes longer than one poll sees no difference.
    loop {
        let resp = cli_http_client()
            .get(&url)
            .timeout(Duration::from_secs(60 * 60 + 60))
            .send()
            .await?;
        let body: WaitResp = ensure_success(resp, "watch").await?.json().await?;
        if body.finished_at.is_some() {
            println!("Review complete.");
            return Ok(());
        }
    }
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
        let c: Comment = ensure_success(resp, "reply").await?.json().await?;
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
    let (prefix, suffix) = context_around(&stripped, &q);
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
    let c: Comment = ensure_success(resp, "comment").await?.json().await?;
    println!("Added comment {}", c.id);
    Ok(())
}

/// How much surrounding text to record on each side of the quote, in bytes.
/// A hint for re-anchoring, not an exact count — snapping to char boundaries can
/// shrink it by a few bytes, which is fine.
const CONTEXT_BYTES: usize = 32;

/// Find the quote in already-stripped text, returning ~32 bytes of surrounding
/// context. Takes the stripped text rather than the raw HTML because the caller
/// has already paid for that pass.
///
/// Both offsets are snapped to char boundaries: an em-dash or emoji within
/// `CONTEXT_BYTES` of the match — routine in a design doc — would otherwise put
/// the slice index mid-codepoint and panic.
fn context_around(text: &str, quote: &str) -> (Option<String>, Option<String>) {
    if let Some(idx) = text.find(quote) {
        let pre_start = floor_char_boundary(text, idx.saturating_sub(CONTEXT_BYTES));
        let quote_end = idx + quote.len();
        let suf_end = ceil_char_boundary(text, quote_end + CONTEXT_BYTES);
        let prefix = Some(text[pre_start..idx].to_string());
        let suffix = Some(text[quote_end..suf_end].to_string());
        (prefix, suffix)
    } else {
        (None, None)
    }
}

/// Largest char boundary `<= i`. (`str::floor_char_boundary` is still unstable.)
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Smallest char boundary `>= i`, clamped to the end of the string.
fn ceil_char_boundary(s: &str, i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    let mut i = i;
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
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
    ensure_success(resp, "unpublish").await?;
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

#[cfg(test)]
mod tests {
    use super::{context_around, parse_port};

    /// The bug this guards: an em-dash within 32 bytes of the match used to put
    /// the slice index mid-codepoint and panic. Design docs are full of them.
    #[test]
    fn context_survives_multibyte_neighbors() {
        let text = "an em—dash and a curly ’quote’ precede TARGET and 😀 emoji follow — really";
        let (prefix, suffix) = context_around(text, "TARGET");
        assert!(prefix.expect("prefix").ends_with("precede "));
        assert!(suffix.expect("suffix").starts_with(" and 😀"));
    }

    /// A multi-byte char straddling the exact 32-byte cut in both directions.
    #[test]
    fn context_snaps_to_char_boundaries_at_the_cut() {
        // Padding chosen so the naive offset lands inside a 3-byte char.
        let text = format!("{}…QUOTE…{}", "x".repeat(30), "y".repeat(30));
        let (prefix, suffix) = context_around(&text, "QUOTE");
        assert!(prefix.expect("prefix").ends_with('…'));
        assert!(suffix.expect("suffix").starts_with('…'));
    }

    #[test]
    fn context_is_none_when_the_quote_is_absent() {
        assert_eq!(context_around("nothing here", "MISSING"), (None, None));
    }

    #[test]
    fn port_zero_is_rejected() {
        assert!(parse_port("0").is_err());
        assert_eq!(parse_port("7321").unwrap(), 7321);
        assert!(parse_port("notanumber").is_err());
    }
}
