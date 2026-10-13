//! Runs `html_to_markdown` in a separate process. The conversion is bounded, but it's still seconds of CPU on a page
//! someone else wrote; a process can be killed at any point, so a page that takes too long is cut off and its plain
//! text used instead.
//!
//! Ports Marv's `src/tools/web/convert.ts`.
//!
//! - One process per page (a fresh one carries nothing over and gives all its memory back: a 5 MB page can take a
//!   gigabyte while it's converted).
//! - The child gets JSON on stdin, an empty environment, and a working folder that isn't the project (Marv: so Bun
//!   wouldn't read the project's `bunfig.toml` / `.env`; marv-rs is one static binary, but the rule stays).
//! - SIGKILLed on timeout ([`CONVERT_TIMEOUT_MS`] = 15 s), abort, completion or Marv's exit; on timeout or failure the
//!   page comes back as plain text (`plain: true`, reason `timeout`/`error`).
//! - marv-rs re-execs its own binary (`std::env::current_exe()`) as the hidden `marv __html-convert`, which calls
//!   [`crate::web::html_convert_cli::main`]. Threads can't be killed in Rust either, which is why this stays a process.
//!   The reply is the last line of stdout. Tests use the test binary itself as the converter ([`Converter`]).

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::types::ToolError;
use crate::web::STOPPED;
use crate::web::html::{FallbackReason, Markdown};
use crate::web::html_convert_cli::{Reply, Request};
use crate::web::plain::{MAX_CHARS, plain_text};

/// The slowest bounded conversion measured takes ~5.5 s on a fast machine; this leaves room for a slower one.
pub const CONVERT_TIMEOUT_MS: u64 = 15_000;

/// The converter process to start: `marv __html-convert` (this very binary), with nothing from Marv's environment.
#[derive(Debug, Clone)]
pub struct Converter {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Tests only: the test binary needs a marker to act as the converter. Production passes none.
    pub env: Vec<(String, String)>,
}

impl Converter {
    /// This binary's own `__html-convert` subcommand.
    pub fn current() -> Result<Self, ToolError> {
        let program = std::env::current_exe().map_err(|e| ToolError(format!("Couldn't start the page converter: {e}.")))?;
        Ok(Self { program, args: vec!["__html-convert".into()], env: Vec::new() })
    }
}

#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// After this long the converter is killed and the page's plain text is returned instead (`plain: true`).
    pub timeout: Duration,
    /// Default: [`Converter::current`].
    pub converter: Option<Converter>,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self { timeout: Duration::from_millis(CONVERT_TIMEOUT_MS), converter: None }
    }
}

static LAST_PID: AtomicU32 = AtomicU32::new(0);
/// Converters still running. If Marv exits mid-conversion (`process::exit` runs no destructor), its children would
/// be orphaned and keep converting for seconds: [`kill_running`] is for the exit path.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// For tests: the pid of the converter the last `convert_html` started.
pub fn last_converter_pid() -> u32 {
    LAST_PID.load(Ordering::SeqCst)
}

/// How many converters are running.
pub fn running_converters() -> usize {
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// SIGKILLs every converter still running (call it on exit).
pub fn kill_running() {
    for pid in RUNNING.lock().unwrap_or_else(|e| e.into_inner()).drain(..) {
        if let Ok(pid) = i32::try_from(pid) {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), nix::sys::signal::Signal::SIGKILL);
        }
    }
}

/// Forgets a converter once it's gone.
struct Tracked(u32);

impl Drop for Tracked {
    fn drop(&mut self) {
        RUNNING.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| *p != self.0);
    }
}

fn parse_reply(out: &str) -> Option<Reply> {
    let line = out.lines().rev().find(|l| !l.trim().is_empty())?;
    match serde_json::from_str::<Reply>(line).ok()? {
        reply @ Reply::Ok { ok: true, .. } => Some(reply),
        reply @ Reply::Err { ok: false, .. } => Some(reply),
        _ => None,
    }
}

/// `html_to_markdown` in its own process. The Markdown, or the page's plain text (`plain: true`, reason `timeout` or
/// `error`) if converting took too long or failed; an error only if the run was stopped or the process couldn't
/// start. Either way the process has exited by the time this returns.
pub async fn convert_html(
    html: &str,
    url: &str,
    cancel: &CancellationToken,
    options: &ConvertOptions,
) -> Result<Markdown, ToolError> {
    convert(html, url, cancel, options, |_| {}).await
}

/// [`convert_html`], telling `started` the converter's pid.
async fn convert(
    html: &str,
    url: &str,
    cancel: &CancellationToken,
    options: &ConvertOptions,
    started: impl FnOnce(u32),
) -> Result<Markdown, ToolError> {
    if cancel.is_cancelled() {
        return Err(ToolError(STOPPED.into()));
    }
    let converter = match &options.converter {
        Some(c) => c.clone(),
        None => Converter::current()?,
    };
    let mut child = tokio::process::Command::new(&converter.program)
        .args(&converter.args)
        // Nothing from Marv's environment: the page decides what this process does, and API keys have no business there.
        .env_clear()
        .envs(converter.env.iter().map(|(k, v)| (k, v)))
        // Not the project's folder (Marv: Bun would read its bunfig.toml and .env; the rule stays).
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError(format!("Couldn't start the page converter: {e}.")))?;
    let pid = child.id().unwrap_or(0);
    LAST_PID.store(pid, Ordering::SeqCst);
    started(pid);
    RUNNING.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
    let _tracked = Tracked(pid);

    let request = serde_json::to_vec(&Request { html: html.to_string(), url: url.to_string() })
        .map_err(|e| ToolError(format!("Couldn't start the page converter: {e}.")))?;
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let exchange = async {
        let write = async {
            if let Some(stdin) = stdin.as_mut() {
                let _ = stdin.write_all(&request).await;
            }
            drop(stdin.take()); // EOF: the converter starts
        };
        let read = async {
            let mut out = String::new();
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.read_to_string(&mut out).await;
            }
            out
        };
        let ((), out) = tokio::join!(write, read);
        out
    };
    let fallback = |reason| Markdown { reason: Some(reason), ..plain_text(html, MAX_CHARS) };
    let outcome = tokio::select! {
        () = cancel.cancelled() => Err(ToolError(STOPPED.into())),
        () = tokio::time::sleep(options.timeout) => Ok(fallback(FallbackReason::Timeout)),
        // stdout closes when the converter exits. A reply that isn't one (it crashed, was killed for memory) means
        // the conversion failed: the plain text is given right away rather than after the timeout.
        out = exchange => Ok(match parse_reply(&out) {
            Some(Reply::Ok { result, .. }) => result,
            _ => fallback(FallbackReason::Error),
        }),
    };
    // Always, and only return once it's gone: nothing is left running, or using memory.
    let _ = child.start_kill();
    let _ = child.wait().await;
    outcome
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::web::html::html_to_markdown;

    const MARKER: &str = "MARV_TEST_HTML_CONVERTER";

    /// The test binary as the converter: it runs only [`converter_process`], which acts as `marv __html-convert`.
    pub(crate) fn test_converter() -> Converter {
        Converter {
            program: std::env::current_exe().unwrap(),
            args: ["--exact", "web::convert::tests::converter_process", "--nocapture", "--test-threads=1", "-q"]
                .map(String::from)
                .to_vec(),
            env: vec![(MARKER.into(), "1".into())],
        }
    }

    pub(crate) fn test_options() -> ConvertOptions {
        ConvertOptions { converter: Some(test_converter()), ..ConvertOptions::default() }
    }

    #[test]
    fn converter_process() {
        if std::env::var_os(MARKER).is_some() {
            let code = i32::from(crate::web::html_convert_cli::main().is_err());
            std::process::exit(code);
        }
    }

    /// Converts, and says which converter did it.
    async fn converted(html: &str, cancel: &CancellationToken, options: &ConvertOptions) -> (Result<Markdown, ToolError>, u32) {
        let mut pid = 0;
        let result = convert(html, "https://e.com/", cancel, options, |p| pid = p).await;
        (result, pid)
    }

    const MB: usize = 1024 * 1024;
    const LOREM: &str = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt. ";

    fn fill(unit: &str, bytes: usize) -> String {
        unit.repeat(bytes.div_ceil(unit.len()))
    }

    fn page(body: &str) -> String {
        format!("<!doctype html><html><head><title>Big</title></head><body>{body}</body></html>")
    }

    /// Whether a process with this pid exists (signal 0 checks without sending anything). A zombie counts as gone.
    fn alive(pid: u32) -> bool {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        !stat.is_empty() && !stat.rsplit(')').next().unwrap_or("").trim_start().starts_with('Z')
    }

    #[tokio::test]
    async fn converts_in_its_own_process_with_the_same_result_as_html_to_markdown() {
        let html =
            page(&format!(r#"<nav>Menu</nav><article><h1>Title</h1><p>{} <a href="/x">x</a></p></article>"#, LOREM.repeat(4)));
        let (result, pid) = converted(&html, &CancellationToken::new(), &test_options()).await;
        assert_eq!(result.unwrap(), html_to_markdown(&html, "https://e.com/"));
        assert!(pid > 0 && !alive(pid)); // it exited
    }

    #[tokio::test]
    async fn an_ordinary_200_kb_page_is_converted_not_given_up_on() {
        let article = fill(
            &format!("<h2>Section</h2><p>{LOREM}<a href=\"/more\">more</a> <code>x</code></p><ul><li>{LOREM}</li></ul>\n"),
            200 * 1024,
        );
        let html = page(&format!(
            "<header><nav><a href=\"/\">Home</a></nav></header><main><article>{article}</article></main><footer>(c)</footer>"
        ));
        let result = convert_html(&html, "https://e.com/", &CancellationToken::new(), &test_options()).await.unwrap();
        assert!(!result.plain);
        assert!(!result.fallback);
    }

    #[tokio::test]
    async fn hostile_pages_are_converted_or_given_as_plain_text_without_blocking() {
        for html in [
            page(&format!("<ul>{}</ul>", fill(r#"<li><a href="/p/x?q=1">item link</a></li>"#, 5 * MB))),
            page(&format!("{}{LOREM}{}", "<div>".repeat(2000), "</div>".repeat(2000))),
            page(&format!("<div>{}</div>", fill("<br>", 5 * MB))),
        ] {
            let options = ConvertOptions { timeout: Duration::from_secs(20), ..test_options() };
            let (result, pid) = converted(&html, &CancellationToken::new(), &options).await;
            assert!(result.unwrap().markdown.chars().count() <= MAX_CHARS);
            assert!(!alive(pid));
        }
    }

    #[tokio::test]
    async fn too_slow_the_converter_is_killed_and_the_pages_plain_text_comes_back() {
        let html = page(&format!(
            "<script>var secret = 1;</script><p hidden>HIDDEN</p><p>Visible &amp; <b>kept</b></p>{}",
            fill(r#"<li><a href="/p">x</a></li>"#, 2 * MB)
        ));
        let options = ConvertOptions { timeout: Duration::from_millis(1), ..test_options() };
        let (result, pid) = converted(&html, &CancellationToken::new(), &options).await;
        let result = result.unwrap();
        assert!(result.plain && result.fallback);
        assert_eq!(result.reason, Some(FallbackReason::Timeout));
        assert_eq!(result.title.as_deref(), Some("Big"));
        assert!(result.markdown.starts_with("Visible & kept"), "{}", &result.markdown[..40]);
        assert!(!result.markdown.contains("secret") && !result.markdown.contains("HIDDEN"));
        assert!(!alive(pid)); // gone by the time it returned, not merely told to stop
    }

    #[tokio::test]
    async fn stopping_kills_the_converter_and_says_so() {
        let html = page(&fill(r#"<li><a href="/p">x</a></li>"#, 5 * MB));
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stop.cancel();
        });
        let (result, pid) = converted(&html, &cancel, &test_options()).await;
        assert_eq!(result.unwrap_err().0, STOPPED);
        assert!(pid > 0 && !alive(pid));
        let (again, started) = converted(&html, &cancel, &test_options()).await;
        assert_eq!(again.unwrap_err().0, STOPPED);
        assert_eq!(started, 0); // already stopped: nothing was started
    }

    #[tokio::test]
    async fn a_converter_that_answers_nonsense_gives_the_plain_text_at_once() {
        let options = ConvertOptions {
            converter: Some(Converter { program: "/bin/echo".into(), args: vec!["not json".into()], env: Vec::new() }),
            ..ConvertOptions::default()
        };
        let result = convert_html("<p>hello</p>", "https://e.com/", &CancellationToken::new(), &options).await.unwrap();
        assert_eq!((result.reason, result.markdown.as_str()), (Some(FallbackReason::Error), "hello"));
    }
}
