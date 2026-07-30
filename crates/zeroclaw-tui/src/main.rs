//! Watch a panel think, and join in.
//!
//! ```text
//! zeroclaw-tui http://127.0.0.1:8088 <conversation-id> [--as <participant-uuid>]
//! ```
//!
//! A projection of the ledger and nothing more: it holds no conversation state,
//! renders what the view API serves, and posts contributions back. Close it,
//! reopen it, run three of them — the conversation is unaffected, because the
//! conversation is not here.
//!
//! Keys: type to compose, `Enter` to send, `Tab` to pass the round, `Esc` or
//! `Ctrl-C` to quit.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::{Terminal, TerminalOptions};
use serde::Deserialize;
use std::io::stdout;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
struct ViewEntry {
    seq: i64,
    round: i64,
    kind: String,
    display_name: String,
    content: String,
}

#[derive(Debug, Clone, Deserialize)]
struct ViewRound {
    round: i64,
    head_seq: i64,
    members: Vec<ViewMember>,
}

#[derive(Debug, Clone, Deserialize)]
struct ViewMember {
    display_name: String,
    kind: String,
    disposition: String,
}

/// Everything on screen, refreshed from the API.
#[derive(Default)]
struct Screen {
    entries: Vec<ViewEntry>,
    round: Option<ViewRound>,
    compose: String,
    status: String,
    scroll: u16,
}

struct Client {
    http: reqwest::Client,
    base: String,
    conversation: String,
    identity: Option<String>,
}

impl Client {
    async fn entries(&self) -> Result<Vec<ViewEntry>, String> {
        self.http
            .get(format!(
                "{}/conversations/{}/entries",
                self.base, self.conversation
            ))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())
    }

    async fn round(&self, round: i64) -> Result<ViewRound, String> {
        self.http
            .get(format!(
                "{}/conversations/{}/round?round={round}",
                self.base, self.conversation
            ))
            .send()
            .await
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())
    }

    async fn contribute(&self, content: &str) -> Result<(), String> {
        let identity = self
            .identity
            .as_deref()
            .ok_or("no --as participant given, so this client can only watch")?;
        let response = self
            .http
            .post(format!(
                "{}/conversations/{}/contribution",
                self.base, self.conversation
            ))
            .json(&serde_json::json!({
                "participant_id": identity,
                "content": content,
            }))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("orchestrator answered {}", response.status()))
        }
    }

    async fn pass(&self) -> Result<(), String> {
        let identity = self
            .identity
            .as_deref()
            .ok_or("no --as participant given, so this client can only watch")?;
        let response = self
            .http
            .post(format!(
                "{}/conversations/{}/pass",
                self.base, self.conversation
            ))
            .json(&serde_json::json!({ "participant_id": identity }))
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(format!("orchestrator answered {}", response.status()))
        }
    }
}

fn colour_for(kind: &str) -> Color {
    // Humans and agents must stay tellable apart at a glance, for the same
    // reason projection keeps them distinct in text.
    match kind {
        "human" => Color::Cyan,
        _ => Color::Green,
    }
}

fn disposition_style(disposition: &str) -> Style {
    match disposition {
        "sealed" => Style::default().fg(Color::Green),
        "passed" => Style::default().fg(Color::DarkGray),
        "timed_out" => Style::default().fg(Color::Yellow),
        "observer" => Style::default().fg(Color::DarkGray),
        _ => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    }
}

fn ledger_lines(screen: &Screen) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut last_round = None;
    for entry in &screen.entries {
        // A round heading, because contributions under one were composed
        // simultaneously and did not see each other. Without it the reader takes
        // sequence for causality.
        if last_round != Some(entry.round) {
            last_round = Some(entry.round);
            lines.push(Line::from(Span::styled(
                format!("── round {} ──", entry.round),
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("{:>4} ", entry.seq),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(
                format!("[{}:{}] ", entry.kind, entry.display_name),
                Style::default()
                    .fg(colour_for(&entry.kind))
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        for text in entry.content.lines() {
            lines.push(Line::from(Span::raw(format!("     {text}"))));
        }
        lines.push(Line::from(""));
    }
    if lines.is_empty() {
        lines.push(Line::from(Span::styled(
            "  (nothing said yet)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn round_lines(screen: &Screen) -> Vec<Line<'static>> {
    let Some(round) = &screen.round else {
        return vec![Line::from(Span::styled(
            "  (no round open)",
            Style::default().fg(Color::DarkGray),
        ))];
    };
    let mut lines = vec![Line::from(Span::styled(
        format!(" round {}  head {}", round.round, round.head_seq),
        Style::default().add_modifier(Modifier::BOLD),
    ))];
    for member in &round.members {
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {:<12}", member.display_name),
                Style::default().fg(colour_for(&member.kind)),
            ),
            Span::styled(
                member.disposition.clone(),
                disposition_style(&member.disposition),
            ),
        ]));
    }
    lines
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let base = args
        .next()
        .ok_or("usage: zeroclaw-tui <orchestrator-url> <conversation-id> [--as <uuid>]")?;
    let conversation = args.next().ok_or("missing conversation id")?;
    let mut identity = None;
    while let Some(flag) = args.next() {
        if flag == "--as" {
            identity = args.next();
        }
    }

    let client = Client {
        http: reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()?,
        base: base.trim_end_matches('/').to_string(),
        conversation,
        identity,
    };

    enable_raw_mode()?;
    crossterm::execute!(stdout(), EnterAlternateScreen)?;
    let mut terminal = Terminal::with_options(
        ratatui::backend::CrosstermBackend::new(stdout()),
        TerminalOptions {
            viewport: ratatui::Viewport::Fullscreen,
        },
    )?;

    let mut screen = Screen {
        status: if client.identity.is_some() {
            "type to compose · Enter send · Tab pass · Esc quit".to_string()
        } else {
            "watching only (pass --as <uuid> to contribute) · Esc quit".to_string()
        },
        ..Screen::default()
    };

    let outcome = run(&mut terminal, &client, &mut screen).await;

    disable_raw_mode()?;
    crossterm::execute!(stdout(), LeaveAlternateScreen)?;
    outcome
}

async fn run(
    terminal: &mut Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    client: &Client,
    screen: &mut Screen,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut last_refresh = std::time::Instant::now() - Duration::from_secs(10);

    loop {
        if last_refresh.elapsed() >= Duration::from_millis(700) {
            last_refresh = std::time::Instant::now();
            match client.entries().await {
                Ok(entries) => screen.entries = entries,
                Err(error) => screen.status = format!("cannot read ledger: {error}"),
            }
            // The current round is one past the last admitted round, or the one
            // in progress; ask for both and keep whichever answers.
            let guess = screen.entries.last().map(|entry| entry.round).unwrap_or(0);
            for candidate in [guess + 1, guess.max(1)] {
                if let Ok(round) = client.round(candidate).await
                    && !round.members.is_empty()
                {
                    screen.round = Some(round);
                    break;
                }
            }
        }

        terminal.draw(|frame| {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(34)])
                .split(frame.area());
            let left = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(5),
                    Constraint::Length(3),
                    Constraint::Length(1),
                ])
                .split(columns[0]);

            let lines = ledger_lines(screen);
            frame.render_widget(
                Paragraph::new(lines)
                    .block(Block::default().borders(Borders::ALL).title(" ledger "))
                    .wrap(Wrap { trim: false })
                    .scroll((screen.scroll, 0)),
                left[0],
            );
            frame.render_widget(
                Paragraph::new(screen.compose.clone())
                    .block(Block::default().borders(Borders::ALL).title(" compose ")),
                left[1],
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    screen.status.clone(),
                    Style::default().fg(Color::DarkGray),
                )),
                left[2],
            );
            frame.render_widget(
                Paragraph::new(round_lines(screen))
                    .block(Block::default().borders(Borders::ALL).title(" round ")),
                columns[1],
            );
        })?;

        if event::poll(Duration::from_millis(120))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Ok(());
                }
                KeyCode::Enter => {
                    let content = screen.compose.trim().to_string();
                    if !content.is_empty() {
                        screen.status = match client.contribute(&content).await {
                            Ok(()) => {
                                screen.compose.clear();
                                "sent — it will land when this round closes".to_string()
                            }
                            Err(error) => format!("could not send: {error}"),
                        };
                    }
                }
                KeyCode::Tab => {
                    screen.status = match client.pass().await {
                        Ok(()) => "passed this round".to_string(),
                        Err(error) => format!("could not pass: {error}"),
                    };
                }
                KeyCode::Backspace => {
                    screen.compose.pop();
                }
                KeyCode::Up => screen.scroll = screen.scroll.saturating_sub(1),
                KeyCode::Down => screen.scroll = screen.scroll.saturating_add(1),
                KeyCode::Char(character) => screen.compose.push(character),
                _ => {}
            }
        }
    }
}
