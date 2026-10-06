//! ai-usage-tui — show AI usage windows as progress bars in the terminal.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use clap::Parser;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Gauge, Paragraph};
use ratatui::Frame;
use serde::Deserialize;

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_TICK: Duration = Duration::from_millis(250);
const BAR_WIDTH: usize = 40;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(name = "ai-usage-tui", about = "Show AI usage as progress bars")]
struct Cli {
    /// Full usage URL (token included in the query string).
    /// Falls back to the AI_USAGE_TUI_URL environment variable.
    #[arg(long, env = "AI_USAGE_TUI_URL")]
    url: Option<String>,

    /// Refresh interval in seconds.
    #[arg(long, default_value_t = 60)]
    interval: u64,

    /// Fetch once, print plain-text bars to stdout, and exit.
    #[arg(long)]
    once: bool,
}

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, PartialEq)]
struct Window {
    used: u8,
    resets_at: i64,
    formatted_message: String,
}

/// One provider as it arrives on the wire: an optional `updated_at` plus any number of
/// other keys. Keys whose value is a JSON object are windows; scalar keys are metadata
/// we don't know about and ignore (the backend has already added one such key once).
#[derive(Debug, Deserialize)]
struct ProviderRaw {
    updated_at: Option<i64>,
    #[serde(flatten)]
    rest: HashMap<String, serde_json::Value>,
}

type Raw = HashMap<String, ProviderRaw>;

#[derive(Debug, Clone, PartialEq)]
struct Provider {
    name: String,
    /// Millisecond epoch of when the backend last refreshed this provider's numbers.
    updated_at: Option<i64>,
    /// Sorted with `five_hours`, `seven_days` first, then alphabetically.
    windows: Vec<(String, Window)>,
}

/// Providers sorted by name.
#[derive(Debug, Clone, PartialEq)]
struct Usage {
    providers: Vec<Provider>,
}

fn window_rank(name: &str) -> (u8, &str) {
    match name {
        "five_hours" => (0, name),
        "seven_days" => (1, name),
        _ => (2, name),
    }
}

#[cfg(test)]
fn parse_usage(json: &str) -> Result<Usage, serde_json::Error> {
    let raw: Raw = serde_json::from_str(json)?;
    Usage::from_raw(raw)
}

impl Usage {
    fn from_raw(raw: Raw) -> Result<Self, serde_json::Error> {
        let mut providers = raw
            .into_iter()
            .map(|(name, p)| {
                let mut windows = p
                    .rest
                    .into_iter()
                    .filter(|(_, v)| v.is_object())
                    .map(|(k, v)| serde_json::from_value::<Window>(v).map(|w| (k, w)))
                    .collect::<Result<Vec<_>, _>>()?;
                windows.sort_by(|(a, _), (b, _)| window_rank(a).cmp(&window_rank(b)));
                Ok(Provider {
                    name,
                    updated_at: p.updated_at,
                    windows,
                })
            })
            .collect::<Result<Vec<_>, serde_json::Error>>()?;
        providers.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Usage { providers })
    }

    fn window_count(&self) -> usize {
        self.providers.iter().map(|p| p.windows.len()).sum()
    }
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn remaining_percent(w: &Window) -> u8 {
    100u8.saturating_sub(w.used)
}

fn strip_slack_markup(s: &str) -> String {
    s.replace('*', "")
}

fn format_reset(resets_at_ms: i64, now: DateTime<Local>) -> String {
    match DateTime::from_timestamp_millis(resets_at_ms) {
        Some(utc) => {
            let local = utc.with_timezone(&Local);
            let delta = local.signed_duration_since(now);
            let countdown = if delta.num_seconds() <= 0 {
                "now".to_string()
            } else {
                let mins = delta.num_minutes();
                let (h, m) = (mins / 60, mins % 60);
                if h > 0 {
                    format!("in {h}h {m}m")
                } else {
                    format!("in {m}m")
                }
            };
            format!("resets {} ({countdown})", local.format("%m-%d %H:%M"))
        }
        None => format!("resets_at={resets_at_ms} (invalid)"),
    }
}

/// Server-side refresh time of a provider, in local time with the date (it may not be today).
fn format_updated_at(updated_at_ms: i64) -> String {
    match DateTime::from_timestamp_millis(updated_at_ms) {
        Some(utc) => utc
            .with_timezone(&Local)
            .format("%m-%d %H:%M:%S")
            .to_string(),
        None => format!("updated_at={updated_at_ms} (invalid)"),
    }
}

fn window_label(w: &Window, now: DateTime<Local>) -> String {
    format!(
        "{}% left | {} | {}",
        remaining_percent(w),
        strip_slack_markup(&w.formatted_message),
        format_reset(w.resets_at, now)
    )
}

fn plain_bar(used: u8) -> String {
    let filled = (usize::from(used.min(100)) * BAR_WIDTH) / 100;
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

fn render_plain(usage: &Usage, now: DateTime<Local>) -> String {
    let mut out = String::new();
    for p in &usage.providers {
        match p.updated_at {
            Some(ts) => out.push_str(&format!("[{}] updated {}\n", p.name, format_updated_at(ts))),
            None => out.push_str(&format!("[{}]\n", p.name)),
        }
        for (name, w) in &p.windows {
            out.push_str(&format!(
                "  {name:<12} {} {}\n",
                plain_bar(w.used),
                window_label(w, now)
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

fn build_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .http_status_as_error(true)
        .build();
    ureq::Agent::new_with_config(config)
}

/// Fetch and parse. Error strings never include the URL (it carries the token).
fn fetch_usage(agent: &ureq::Agent, url: &str) -> Result<Usage, String> {
    let mut resp = agent
        .get(url)
        .call()
        .map_err(|e| format!("request failed: {e}"))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("http status: {status}"));
    }
    let raw: Raw = resp
        .body_mut()
        .read_json()
        .map_err(|e| format!("invalid response body: {e}"))?;
    Usage::from_raw(raw).map_err(|e| format!("invalid response body: {e}"))
}

// ---------------------------------------------------------------------------
// TUI
// ---------------------------------------------------------------------------

struct App {
    usage: Option<Usage>,
    last_updated: Option<DateTime<Local>>,
    last_error: Option<String>,
    interval: Duration,
}

impl App {
    fn new(interval: Duration) -> Self {
        App {
            usage: None,
            last_updated: None,
            last_error: None,
            interval,
        }
    }

    fn apply(&mut self, result: Result<Usage, String>) {
        match result {
            Ok(usage) => {
                self.usage = Some(usage);
                self.last_updated = Some(Local::now());
                self.last_error = None;
            }
            Err(e) => self.last_error = Some(e),
        }
    }
}

fn render(frame: &mut Frame, app: &App, now: DateTime<Local>) {
    let area = frame.area();
    let [body, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    match &app.usage {
        Some(usage) if usage.window_count() > 0 => render_usage(frame, body, usage, app, now),
        Some(_) => frame.render_widget(Paragraph::new("No usage windows in response."), body),
        None => frame.render_widget(Paragraph::new("Waiting for first fetch..."), body),
    }

    let fetched = app
        .last_updated
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "never".to_string());
    let footer_text = match &app.last_error {
        Some(e) => Line::styled(
            format!("fetched {fetched}  |  ERROR: {e}  (q / Esc / Ctrl-C to quit)"),
            Color::Red,
        ),
        None => Line::styled(
            format!(
                "fetched {fetched}  |  refresh every {}s  |  q / Esc / Ctrl-C to quit",
                app.interval.as_secs()
            ),
            Color::DarkGray,
        ),
    };
    frame.render_widget(Paragraph::new(footer_text), footer);
}

fn render_usage(frame: &mut Frame, area: Rect, usage: &Usage, app: &App, now: DateTime<Local>) {
    // Fallback title time when the backend doesn't say when its numbers were refreshed.
    let fetched = app
        .last_updated
        .map(|t| t.format("%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| "never".to_string());

    // One block per provider; each window is a 3-row gauge inside.
    let provider_heights: Vec<Constraint> = usage
        .providers
        .iter()
        .map(|p| Constraint::Length(u16::try_from(p.windows.len() * 3 + 2).unwrap_or(u16::MAX)))
        .collect();
    let provider_areas = Layout::vertical(provider_heights).split(area);

    for (p, provider_area) in usage.providers.iter().zip(provider_areas.iter()) {
        let title = match p.updated_at {
            Some(ts) => format!(" {} — updated {} ", p.name, format_updated_at(ts)),
            None => format!(" {} — fetched {fetched} ", p.name),
        };
        let block = Block::default().borders(Borders::ALL).title(title);
        let inner = block.inner(*provider_area);
        frame.render_widget(block, *provider_area);

        let rows = Layout::vertical(p.windows.iter().map(|_| Constraint::Length(3))).split(inner);
        for ((name, w), row) in p.windows.iter().zip(rows.iter()) {
            let used = w.used.min(100);
            let color = match used {
                0..=59 => Color::Green,
                60..=84 => Color::Yellow,
                _ => Color::Red,
            };
            let gauge = Gauge::default()
                .block(Block::default().borders(Borders::ALL).title(name.as_str()))
                .gauge_style(Style::default().fg(color).bg(Color::Black))
                .percent(u16::from(used))
                .label(window_label(w, now));
            frame.render_widget(gauge, *row);
        }
    }
}

fn run_tui(
    terminal: &mut ratatui::DefaultTerminal,
    agent: &ureq::Agent,
    url: &str,
    interval: Duration,
) -> std::io::Result<()> {
    let mut app = App::new(interval);
    app.apply(fetch_usage(agent, url));
    let mut next_fetch = Instant::now() + interval;

    loop {
        terminal.draw(|f| render(f, &app, Local::now()))?;

        let wait = next_fetch.saturating_duration_since(Instant::now()).min(POLL_TICK);
        if event::poll(wait)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            let ctrl_c =
                key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
            if matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) || ctrl_c {
                return Ok(());
            }
        }

        if Instant::now() >= next_fetch {
            app.apply(fetch_usage(agent, url));
            next_fetch = Instant::now() + interval;
        }
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> ExitCode {
    let cli = Cli::parse();

    let Some(url) = cli.url.filter(|u| !u.trim().is_empty()) else {
        eprintln!("error: no usage URL given. Pass --url <URL> or set the AI_USAGE_TUI_URL environment variable.");
        return ExitCode::from(2);
    };
    let agent = build_agent();

    if cli.once {
        return match fetch_usage(&agent, &url) {
            Ok(usage) => {
                print!("{}", render_plain(&usage, Local::now()));
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        };
    }

    let interval = Duration::from_secs(cli.interval.max(1));
    let mut terminal = ratatui::init();
    let result = run_tui(&mut terminal, &agent, &url, interval);
    ratatui::restore();
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    const SAMPLE: &str = r#"{
      "claude": {
        "seven_days": { "used": 15, "resets_at": 1791241199681, "formatted_message": "*85%* remaining, resets 10-06 06:59" },
        "one_day":    { "used": 50, "resets_at": 1791241199681, "formatted_message": "*50%* remaining" },
        "five_hours": { "used": 23, "resets_at": 1791196799681, "formatted_message": "*77%* remaining, resets 10-05 18:39" }
      },
      "openai": {
        "five_hours": { "used": 5, "resets_at": 1791196799681, "formatted_message": "*95%* remaining" }
      },
      "bard": {
        "zeta": { "used": 1, "resets_at": 1791196799681, "formatted_message": "x" },
        "alpha": { "used": 2, "resets_at": 1791196799681, "formatted_message": "y" }
      }
    }"#;

    /// Exact payload the backend sends since 2026-10-06: `updated_at` sits next to the windows.
    const SAMPLE_WITH_UPDATED_AT: &str = r#"{
      "claude": {
        "five_hours": { "used": 1, "resets_at": 1791283199962, "formatted_message": "*99%* remaining, resets 10-06 18:39" },
        "seven_days": { "used": 4, "resets_at": 1791845999962, "formatted_message": "*96%* remaining, resets 10-13 06:59" },
        "updated_at": 1791268244117
      }
    }"#;

    fn buffer_text(buf: &ratatui::buffer::Buffer) -> String {
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    + "\n"
            })
            .collect()
    }

    fn window_names(p: &Provider) -> Vec<&str> {
        p.windows.iter().map(|(n, _)| n.as_str()).collect()
    }

    #[test]
    fn parses_and_sorts() {
        let usage = parse_usage(SAMPLE).expect("parse");
        let providers: Vec<&str> = usage.providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(providers, ["bard", "claude", "openai"]);

        assert_eq!(
            window_names(&usage.providers[1]),
            ["five_hours", "seven_days", "one_day"]
        );
        assert_eq!(window_names(&usage.providers[0]), ["alpha", "zeta"]);

        let five = &usage.providers[1].windows[0].1;
        assert_eq!(five.used, 23);
        assert_eq!(five.resets_at, 1791196799681);
        assert_eq!(remaining_percent(five), 77);

        // Old format: no provider-level updated_at.
        assert!(usage.providers.iter().all(|p| p.updated_at.is_none()));
    }

    #[test]
    fn parses_provider_level_updated_at() {
        let usage = parse_usage(SAMPLE_WITH_UPDATED_AT).expect("parse");
        assert_eq!(usage.providers.len(), 1);
        let claude = &usage.providers[0];
        assert_eq!(claude.name, "claude");
        assert_eq!(claude.updated_at, Some(1791268244117));
        // updated_at must not leak in as a window.
        assert_eq!(window_names(claude), ["five_hours", "seven_days"]);
        assert_eq!(claude.windows[0].1.used, 1);
        assert_eq!(claude.windows[1].1.used, 4);
    }

    #[test]
    fn ignores_unknown_scalar_metadata_but_rejects_bad_windows() {
        let ok = r#"{"claude":{"plan":"pro","count":3,"five_hours":{"used":9,"resets_at":1,"formatted_message":"m"}}}"#;
        let usage = parse_usage(ok).expect("parse");
        assert_eq!(window_names(&usage.providers[0]), ["five_hours"]);

        // An object that is not a window is a format drift we want to see, not hide.
        let bad = r#"{"claude":{"five_hours":{"used":9,"resets_at":1,"formatted_message":"m"},"meta":{"x":1}}}"#;
        assert!(parse_usage(bad).is_err());
    }

    #[test]
    fn updated_at_uses_millis() {
        // 2026-10-06T06:30:44Z; date must not drift by decades if millis were read as seconds.
        let s = format_updated_at(1791268244117);
        assert!(s.starts_with("10-0"), "got {s}");
        assert!(s.ends_with(":44"), "got {s}");
    }

    #[test]
    fn reset_time_uses_millis() {
        let s = format_reset(1791196799681, Local::now());
        // 2026-10-05T10:39:59Z; the local hour depends on the machine TZ, the day/month do not
        // drift by a year if millis were mistaken for seconds.
        assert!(s.starts_with("resets 10-0"), "got {s}");
        assert!(!s.contains("invalid"));
    }

    #[test]
    fn renders_gauge_labels() {
        let usage = parse_usage(SAMPLE).expect("parse");
        let mut app = App::new(Duration::from_secs(60));
        app.apply(Ok(usage));
        app.last_error = Some("boom".to_string());

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render(f, &app, Local::now()))
            .expect("draw");

        let text = buffer_text(terminal.backend().buffer());

        assert!(text.contains("77% left"), "missing 77% in:\n{text}");
        assert!(text.contains("85% left"), "missing 85% in:\n{text}");
        assert!(
            text.contains("claude — fetched"),
            "missing provider title in:\n{text}"
        );
        let footer = format!(
            "fetched {}  |  ERROR: boom",
            app.last_updated.expect("fetched").format("%H:%M:%S")
        );
        assert!(text.contains(&footer), "missing {footer:?} in:\n{text}");
        assert!(!text.contains('*'), "slack markup not stripped:\n{text}");
    }

    #[test]
    fn renders_server_updated_at_in_title_and_fetch_time_in_footer() {
        let usage = parse_usage(SAMPLE_WITH_UPDATED_AT).expect("parse");
        let mut app = App::new(Duration::from_secs(60));
        app.apply(Ok(usage));

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|f| render(f, &app, Local::now()))
            .expect("draw");
        let text = buffer_text(terminal.backend().buffer());

        let expected = format!("claude — updated {}", format_updated_at(1791268244117));
        assert!(text.contains(&expected), "missing {expected:?} in:\n{text}");
        let footer = format!(
            "fetched {}  |  refresh every 60s",
            app.last_updated.expect("fetched").format("%H:%M:%S")
        );
        assert!(text.contains(&footer), "missing {footer:?} in:\n{text}");
        assert!(
            text.contains("99% left") && text.contains("96% left"),
            "{text}"
        );
    }

    #[test]
    fn plain_output_has_bars() {
        let usage = parse_usage(SAMPLE).expect("parse");
        let out = render_plain(&usage, Local::now());
        assert!(out.contains('█') && out.contains('░'));
        assert!(out.contains("77% left"));
        assert!(out.contains("85% left"));
        assert!(
            out.contains("[claude]\n"),
            "no updated_at → bare header:\n{out}"
        );

        let usage = parse_usage(SAMPLE_WITH_UPDATED_AT).expect("parse");
        let out = render_plain(&usage, Local::now());
        let expected = format!("[claude] updated {}\n", format_updated_at(1791268244117));
        assert!(out.contains(&expected), "missing {expected:?} in:\n{out}");
    }
}
