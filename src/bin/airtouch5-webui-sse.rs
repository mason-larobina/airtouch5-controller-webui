//! `airtouch5-webui-sse` binary: a tiny SSE listener for debugging.
//!
//! Connects to a running `airtouch5-webui` / `airtouch5-webui-mock` server's
//! `/events` stream and pretty-prints every Server-Sent Event as it arrives.
//! Handy for watching what the server pushes in response to an interaction
//! (e.g. confirming that applying a preset really re-emits the affected
//! `zone-<id>` / `ac-<id>` fragments).
//!
//! Usage:
//!   airtouch5-webui-sse                       # 127.0.0.1:3000/events, compact
//!   airtouch5-webui-sse --addr 127.0.0.1:8111
//!   airtouch5-webui-sse --level full          # print the whole HTML fragment
//!   airtouch5-webui-sse --level names         # just event names
//!   airtouch5-webui-sse --timeout 5           # exit after 5s (prints a summary)

use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::time::Duration;

use clap::{Parser, ValueEnum};
use futures_util::StreamExt;

/// How much of each event to print.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Level {
    /// One line per event: just the event name.
    Names,
    /// One line per event: name, byte size, and a single-lined data preview.
    Compact,
    /// The event name plus the full (multi-line) data payload.
    Full,
}

/// airtouch5-webui-sse: connect to a server's `/events` SSE stream and print events.
#[derive(Parser, Debug)]
#[command(
    name = "airtouch5-webui-sse",
    version,
    about = "Listen to an airtouch5-webui SSE stream and pretty-print events"
)]
struct Cli {
    /// Server address to connect to (host:port).
    #[arg(long, default_value = "127.0.0.1:3000")]
    addr: String,

    /// Request path for the SSE stream.
    #[arg(long, default_value = "/events")]
    path: String,

    /// How much of each event to print.
    #[arg(long, value_enum, default_value_t = Level::Compact)]
    level: Level,

    /// Exit after this many seconds (off by default -- run until Ctrl-C).
    #[arg(long)]
    timeout: Option<u64>,

    /// Truncate the data preview to this many characters (compact level only).
    #[arg(long, default_value_t = 100)]
    width: usize,

    /// Force-disable ANSI colour (otherwise auto-detected from the terminal).
    #[arg(long)]
    no_color: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let color = !cli.no_color && std::io::stdout().is_terminal();

    if let Err(e) = run(&cli, color).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

async fn run(cli: &Cli, color: bool) -> Result<(), Box<dyn std::error::Error>> {
    let url = format!("http://{}{}", cli.addr, cli.path);
    eprintln!("connecting to {url} ...");

    // No client-level timeout: the SSE stream is intentionally long-lived.
    let client = reqwest::Client::builder().build()?;
    let resp = client
        .get(&url)
        .header("Accept", "text/event-stream")
        .send()
        .await?;
    if !resp.status().is_success() {
        return Err(format!("unexpected HTTP status: {}", resp.status()).into());
    }
    eprintln!("connected; listening for events (Ctrl-C to stop)\n");
    let mut stream = resp.bytes_stream();

    // A `pending()` future never resolves, so an unset --timeout means the
    // deadline arm of the select below can never fire.
    let deadline = async {
        match cli.timeout {
            Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::pin!(deadline);

    let mut counts: BTreeMap<String, u64> = BTreeMap::new();
    let mut total = 0u64;
    // Decoded body bytes not yet split into a complete `\n\n`-delimited frame.
    let mut buf: Vec<u8> = Vec::new();

    loop {
        tokio::select! {
            biased;
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\ninterrupted.");
                break;
            }
            _ = &mut deadline => {
                eprintln!("\ntimeout reached.");
                break;
            }
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        buf.extend_from_slice(&bytes);
                        for ev in drain_frames(&mut buf) {
                            total += 1;
                            *counts.entry(ev.label()).or_default() += 1;
                            print_event(&ev, cli.level, cli.width, color);
                        }
                    }
                    Some(Err(e)) => {
                        eprintln!("\nstream error: {e}");
                        break;
                    }
                    None => {
                        eprintln!("\nstream closed by server.");
                        break;
                    }
                }
            }
        }
    }

    print_summary(&counts, total, color);
    Ok(())
}

/// A parsed SSE frame: an optional `event:` name plus the joined `data:` lines.
/// A frame carrying only a `:` comment (keep-alive) is flagged as a `comment`.
struct SseEvent {
    name: Option<String>,
    data: String,
    comment: bool,
}

impl SseEvent {
    /// A stable label for counting/printing ("<keep-alive>" for comment frames,
    /// "<unnamed>" for a dataful frame with no event field).
    fn label(&self) -> String {
        if self.comment {
            "<keep-alive>".to_string()
        } else {
            self.name.clone().unwrap_or_else(|| "<unnamed>".to_string())
        }
    }
}

/// Pull every complete `\n\n`-delimited frame out of `buf`, leaving any trailing
/// partial frame behind for the next chunk.
fn drain_frames(buf: &mut Vec<u8>) -> Vec<SseEvent> {
    let mut out = Vec::new();
    while let Some(end) = buf.windows(2).position(|w| w == b"\n\n") {
        let raw: Vec<u8> = buf.drain(..end + 2).collect();
        if let Some(ev) = parse_frame(&raw) {
            out.push(ev);
        }
    }
    out
}

/// Parse a raw frame (bytes up to and including its `\n\n` delimiter) into an
/// `SseEvent`. Returns `None` for an entirely empty frame.
fn parse_frame(raw: &[u8]) -> Option<SseEvent> {
    let text = String::from_utf8_lossy(raw);
    let mut name: Option<String> = None;
    let mut data = String::new();
    let mut comment = false;
    let mut any = false;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            continue;
        }
        any = true;
        if let Some(rest) = line.strip_prefix(':') {
            comment = true;
            if !rest.is_empty() {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.trim_start());
            }
            continue;
        }
        let (field, value) = match line.split_once(':') {
            Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
            None => (line, ""),
        };
        match field {
            "event" => name = Some(value.to_string()),
            "data" => {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value);
            }
            _ => {}
        }
    }

    if !any {
        return None;
    }
    let comment = comment && name.is_none();
    Some(SseEvent {
        name,
        data,
        comment,
    })
}

fn print_event(ev: &SseEvent, level: Level, width: usize, color: bool) {
    // Keep-alive comments are noise; only surface them at the Full level.
    if ev.comment && level != Level::Full {
        return;
    }

    let ts = now_hms();
    let label = ev.label();
    let name_col = paint(&label, color_for(&label), color);

    match level {
        Level::Names => {
            println!("{}  {}", dim(&ts, color), name_col);
        }
        Level::Compact => {
            let bytes = ev.data.len();
            let preview = single_line(&ev.data, width);
            println!(
                "{}  {:<14} {:>5}B  {}",
                dim(&ts, color),
                name_col,
                bytes,
                dim(&preview, color),
            );
        }
        Level::Full => {
            println!(
                "{}  {} {}",
                dim(&ts, color),
                name_col,
                dim(&format!("({} bytes)", ev.data.len()), color),
            );
            if ev.data.is_empty() {
                println!("  {}", dim("(no data)", color));
            } else {
                for line in ev.data.lines() {
                    println!("  {line}");
                }
            }
            println!();
        }
    }
}

fn print_summary(counts: &BTreeMap<String, u64>, total: u64, color: bool) {
    if total == 0 {
        eprintln!("no events received.");
        return;
    }
    eprintln!("\n{} ({total} total):", paint("summary", "1", color));
    for (name, n) in counts {
        eprintln!("  {:<16} {n}", paint(name, color_for(name), color));
    }
}

// ---------------------------------------------------------------------------
// Presentation helpers
// ---------------------------------------------------------------------------

/// Current wall-clock time as `HH:MM:SS.mmm`.
fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S%.3f").to_string()
}

/// Collapse a multi-line fragment to a single truncated line for the preview.
fn single_line(s: &str, width: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > width {
        let mut out: String = flat.chars().take(width.saturating_sub(1)).collect();
        out.push('\u{2026}'); // horizontal ellipsis
        out
    } else {
        flat
    }
}

/// A distinct ANSI colour code per event family so the stream is scannable.
fn color_for(label: &str) -> &'static str {
    if label.starts_with("zone-") {
        "36" // cyan
    } else if label.starts_with("ac-") {
        "33" // yellow
    } else if label == "presets" {
        "35" // magenta
    } else if label == "automation" {
        "32" // green
    } else if label == "system" || label == "state" {
        "34" // blue
    } else {
        "90" // bright black (keep-alive / unnamed)
    }
}

/// Wrap `s` in an ANSI SGR colour when `color` is enabled.
fn paint(s: &str, code: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[{code}m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}

/// Dim (faint) text when colour is enabled.
fn dim(s: &str, color: bool) -> String {
    if color {
        format!("\u{1b}[2m{s}\u{1b}[0m")
    } else {
        s.to_string()
    }
}
