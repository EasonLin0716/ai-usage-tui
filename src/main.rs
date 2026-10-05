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

type Raw = HashMap<String, HashMap<String, Window>>;

/// Providers sorted by name; windows sorted with `five_hours`, `seven_days` first.
#[derive(Debug, Clone, PartialEq)]
struct Usage {
    providers: Vec<(String, Vec<(String, Window)>)>,
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
    Ok(Usage::from_raw(raw))
}

impl Usage {
    fn from_raw(raw: Raw) -> Self {
        let mut providers: Vec<(String, Vec<(String, Window)>)> = raw
            .into_iter()
            .map(|(provider, windows)| {
                let mut windows: Vec<(String, Window)> = windows.into_iter().collect();
                windows.sort_by(|(a, _), (b, _)| window_rank(a).cmp(&window_rank(b)));
                (provider, windows)
            })
            .collect();
        providers.sort_by(|(a, _), (b, _)| a.cmp(b));
        Usage { providers }
    }

    fn window_count(&self) -> usize {
        self.providers.iter().map(|(_, w)| w.len()).sum()
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
    for (provider, windows) in &usage.providers {
        out.push_str(&format!("[{provider}]\n"));
        for (name, w) in windows {
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
    Ok(Usage::from_raw(raw))
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

    let footer_text = match &app.last_error {
        Some(e) => Line::styled(format!("ERROR: {e}  (q / Esc / Ctrl-C to quit)"), Color::Red),
        None => Line::styled(
            format!(
                "refresh every {}s  |  q / Esc / Ctrl-C to quit",
                app.interval.as_secs()
            ),
            Color::DarkGray,
        ),
    };
    frame.render_widget(Paragraph::new(footer_text), footer);
}

fn render_usage(frame: &mut Frame, area: Rect, usage: &Usage, app: &App, now: DateTime<Local>) {
    let updated = app
        .last_updated
        .map(|t| t.format("%H:%M:%S").to_string())
        .unwrap_or_else(|| "never".to_string());

    // One block per provider; each window is a 3-row gauge inside.
    let provider_heights: Vec<Constraint> = usage
        .providers
        .iter()
        .map(|(_, w)| Constraint::Length(u16::try_from(w.len() * 3 + 2).unwrap_or(u16::MAX)))
        .collect();
    let provider_areas = Layout::vertical(provider_heights).split(area);

    for ((provider, windows), provider_area) in usage.providers.iter().zip(provider_areas.iter()) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {provider} — updated {updated} "));
        let inner = block.inner(*provider_area);
        frame.render_widget(block, *provider_area);

        let rows = Layout::vertical(windows.iter().map(|_| Constraint::Length(3))).split(inner);
        for ((name, w), row) in windows.iter().zip(rows.iter()) {
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

    #[test]
    fn parses_and_sorts() {
        let usage = parse_usage(SAMPLE).expect("parse");
        let providers: Vec<&str> = usage.providers.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(providers, ["bard", "claude", "openai"]);

        let claude: Vec<&str> = usage.providers[1].1.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(claude, ["five_hours", "seven_days", "one_day"]);

        let bard: Vec<&str> = usage.providers[0].1.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(bard, ["alpha", "zeta"]);

        let five = &usage.providers[1].1[0].1;
        assert_eq!(five.used, 23);
        assert_eq!(five.resets_at, 1791196799681);
        assert_eq!(remaining_percent(five), 77);
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

        let buf = terminal.backend().buffer();
        let text: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "))
                    .collect::<String>()
                    + "\n"
            })
            .collect();

        assert!(text.contains("77% left"), "missing 77% in:\n{text}");
        assert!(text.contains("85% left"), "missing 85% in:\n{text}");
        assert!(text.contains("claude"), "missing provider title in:\n{text}");
        assert!(text.contains("ERROR: boom"), "missing footer error in:\n{text}");
        assert!(!text.contains('*'), "slack markup not stripped:\n{text}");
    }

    #[test]
    fn plain_output_has_bars() {
        let usage = parse_usage(SAMPLE).expect("parse");
        let out = render_plain(&usage, Local::now());
        assert!(out.contains('█') && out.contains('░'));
        assert!(out.contains("77% left"));
        assert!(out.contains("85% left"));
    }
}
