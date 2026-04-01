use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::Result;
use chrono::{Local, Utc};
use crossterm::{
    event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState},
    Frame, Terminal,
};
use tokio::sync::mpsc;
use tokio::time;

use crate::state::{MeetingState, Participant, QualitySnapshot};

#[derive(PartialEq, Clone, Copy)]
enum Focus {
    Participants,
    Events,
}

// ── Terminal setup / teardown ─────────────────────────────────────────────────

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Ok(Terminal::new(backend)?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    Ok(())
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub async fn run(mut rx: mpsc::Receiver<Vec<u8>>, meeting_id: String, use_utc: bool) -> Result<()> {
    let mut terminal = setup_terminal()?;
    let result = run_inner(&mut terminal, &mut rx, meeting_id, use_utc).await;
    restore_terminal(&mut terminal)?;
    result
}

async fn run_inner(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    rx: &mut mpsc::Receiver<Vec<u8>>,
    meeting_id: String,
    use_utc: bool,
) -> Result<()> {
    let mut state = MeetingState::new(meeting_id);
    let mut table_state = TableState::default();
    let mut event_stream = EventStream::new();
    let mut render_tick = time::interval(Duration::from_millis(100));
    let mut stale_tick = time::interval(Duration::from_secs(1));
    let mut show_help = false;
    let mut sort_by_quality = false; // Default to alphabetical
    let mut focus = Focus::Participants;
    // event_scroll: lines scrolled back from the bottom (0 = auto-scroll to newest)
    let mut event_scroll: usize = 0;
    let mut prev_event_count: usize = 0;

    loop {
        tokio::select! {
            // Incoming packet from the meeting
            maybe_raw = rx.recv() => {
                match maybe_raw {
                    Some(raw) => {
                        state.process_packet(&raw);
                        // Auto-scroll to bottom when new events arrive, unless user scrolled up
                        if state.events.len() > prev_event_count && event_scroll == 0 {
                            // already at bottom, stays there
                        }
                        prev_event_count = state.events.len();
                    }
                    None => break, // connection closed
                }
            }

            // Keyboard input
            maybe_event = event_stream.next() => {
                match maybe_event {
                    Some(Ok(Event::Key(key))) => {
                        if handle_key(key, &mut table_state, &state, &mut show_help, &mut sort_by_quality, &mut focus, &mut event_scroll) {
                            break;
                        }
                    }
                    Some(Err(_)) | None => break,
                    _ => {}
                }
            }

            // Prune stale participants
            _ = stale_tick.tick() => {
                state.tick();
                clamp_selection(&mut table_state, state.participants.len());
            }

            // Render
            _ = render_tick.tick() => {
                terminal.draw(|f| render(f, &state, &mut table_state, use_utc, show_help, sort_by_quality, focus, event_scroll))?;
            }
        }
    }

    Ok(())
}

fn handle_key(
    key: KeyEvent,
    table_state: &mut TableState,
    state: &MeetingState,
    show_help: &mut bool,
    sort_by_quality: &mut bool,
    focus: &mut Focus,
    event_scroll: &mut usize,
) -> bool {
    match key.code {
        KeyCode::Char('q') => return true,
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => return true,
        KeyCode::Esc => return true,
        KeyCode::Char('h') | KeyCode::Char('?') => {
            *show_help = !*show_help;
        }
        KeyCode::Tab => {
            *focus = match *focus {
                Focus::Participants => Focus::Events,
                Focus::Events => Focus::Participants,
            };
        }
        KeyCode::Char('s') | KeyCode::Char('S') => {
            *sort_by_quality = !*sort_by_quality;
            if !state.participants.is_empty() {
                table_state.select(Some(0));
            }
        }
        KeyCode::Down | KeyCode::Char('j') => {
            match *focus {
                Focus::Participants => {
                    let n = state.participants.len();
                    if n > 0 {
                        let next = match table_state.selected() {
                            Some(i) => (i + 1).min(n - 1),
                            None => 0,
                        };
                        table_state.select(Some(next));
                    }
                }
                Focus::Events => {
                    // Scroll toward newer events (reduce offset, min 0)
                    *event_scroll = event_scroll.saturating_sub(1);
                }
            }
        }
        KeyCode::Up | KeyCode::Char('k') => {
            match *focus {
                Focus::Participants => {
                    let prev = match table_state.selected() {
                        Some(0) | None => 0,
                        Some(i) => i - 1,
                    };
                    table_state.select(Some(prev));
                }
                Focus::Events => {
                    // Scroll toward older events (increase offset, capped by history)
                    let max_scroll = state.events.len().saturating_sub(1);
                    *event_scroll = (*event_scroll + 1).min(max_scroll);
                }
            }
        }
        KeyCode::Home => {
            match *focus {
                Focus::Participants => table_state.select(Some(0)),
                Focus::Events => {
                    // Jump to oldest events
                    *event_scroll = state.events.len().saturating_sub(1);
                }
            }
        }
        KeyCode::End => {
            match *focus {
                Focus::Participants => {
                    let n = state.participants.len();
                    if n > 0 {
                        table_state.select(Some(n - 1));
                    }
                }
                Focus::Events => {
                    // Jump to newest events
                    *event_scroll = 0;
                }
            }
        }
        _ => {}
    }
    false
}

fn clamp_selection(table_state: &mut TableState, len: usize) {
    if let Some(i) = table_state.selected() {
        if len == 0 {
            table_state.select(None);
        } else if i >= len {
            table_state.select(Some(len - 1));
        }
    }
}

// ── Top-level render ──────────────────────────────────────────────────────────

fn render(
    f: &mut Frame,
    state: &MeetingState,
    table_state: &mut TableState,
    use_utc: bool,
    show_help: bool,
    sort_by_quality: bool,
    focus: Focus,
    event_scroll: usize,
) {
    let area = f.area();

    // Require minimum terminal size to avoid panics from zero-height areas
    if area.height < 10 || area.width < 40 {
        let msg = Paragraph::new("Terminal too small — resize to continue")
            .style(Style::default().fg(Color::Yellow));
        f.render_widget(msg, area);
        return;
    }

    let participants = state.sorted_participants(sort_by_quality);

    // Adaptive detail height: show it only when someone is selected
    let detail_h = if table_state.selected().is_some() {
        7u16
    } else {
        0u16
    };
    // Events panel: taller when focused so there's room to scroll
    let events_h = if focus == Focus::Events {
        Constraint::Min(10)
    } else {
        Constraint::Length(6)
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),        // header
            Constraint::Min(4),           // participant table
            Constraint::Length(detail_h), // detail panel
            events_h,                     // events (expands when focused)
            Constraint::Length(1),        // footer
        ])
        .split(area);

    render_header(f, chunks[0], state, use_utc, sort_by_quality);
    render_participants(f, chunks[1], state, &participants, table_state);
    if detail_h > 0 {
        render_detail(f, chunks[2], &participants, table_state.selected());
    }
    render_events(f, chunks[3], state, use_utc, focus, event_scroll);
    render_footer(f, chunks[4], focus);

    // Overlay help screen if active
    if show_help {
        render_help(f, area);
    }
}

// ── Header ────────────────────────────────────────────────────────────────────

fn render_header(
    f: &mut Frame,
    area: Rect,
    state: &MeetingState,
    use_utc: bool,
    sort_by_quality: bool,
) {
    let now = if use_utc {
        Utc::now().format("%H:%M:%S UTC").to_string()
    } else {
        Local::now().format("%H:%M:%S %Z").to_string()
    };

    let sort_mode = if sort_by_quality {
        "Quality ↓"
    } else {
        "Name"
    };

    let title = Line::from(vec![
        Span::styled(
            " vcprobe proctor ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("── "),
        Span::styled(
            &state.meeting_id,
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!("  ({})", state.elapsed_str())),
    ]);

    let time_line = Line::from(vec![
        Span::raw(" "),
        Span::styled(now, Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "   {} participant{}  ─ Sort: ",
            state.participants.len(),
            if state.participants.len() == 1 {
                ""
            } else {
                "s"
            }
        )),
        Span::styled(sort_mode, Style::default().fg(Color::Yellow)),
        Span::styled(" [S]", Style::default().fg(Color::DarkGray)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let header_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Length(1)])
        .split(inner);

    f.render_widget(Paragraph::new(title), header_chunks[0]);
    f.render_widget(Paragraph::new(time_line), header_chunks[1]);
}

// ── Participant table ─────────────────────────────────────────────────────────

fn render_participants(
    f: &mut Frame,
    area: Rect,
    state: &MeetingState,
    participants: &[&Participant],
    table_state: &mut TableState,
) {
    // Two-row grouped header
    // Columns: name | status | RTT | Conc/s | FPS | kbps | Aud | Vid | Call | bar
    let header_group = Row::new(vec![
        Cell::from("Participant"),
        Cell::from("Status"),
        Cell::from("Connection").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Audio").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Video").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Quality Scores").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from(""),
        Cell::from(""),
        Cell::from(""),
    ])
    .style(Style::default().fg(Color::Yellow))
    .height(1);

    let header_cols = Row::new(vec![
        Cell::from(""),
        Cell::from(""),
        Cell::from("WT").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("RTT").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Conc/s").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("FPS").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("kbps").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Aud").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Vid").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Call").style(Style::default().add_modifier(Modifier::BOLD)),
        Cell::from("Bar").style(Style::default().add_modifier(Modifier::BOLD)),
    ])
    .style(Style::default().fg(Color::Yellow))
    .height(1);

    let rows: Vec<Row> = participants.iter().map(|p| participant_row(p)).collect();

    let title = format!(" Participants ({}) ", state.participants.len());

    let table = Table::new(
        rows,
        [
            Constraint::Min(52),    // name (display | user_id | s:XXXX)
            Constraint::Length(11), // [V][M]
            Constraint::Length(3),  // WT indicator
            Constraint::Length(8),  // RTT
            Constraint::Length(8),  // concealment/s
            Constraint::Length(5),  // FPS
            Constraint::Length(6),  // kbps
            Constraint::Length(5),  // Aud score
            Constraint::Length(5),  // Vid score
            Constraint::Length(5),  // Call score
            Constraint::Length(12), // quality bar
        ],
    )
    .header(header_group)
    .header(header_cols)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(title),
    )
    .row_highlight_style(
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
    .highlight_symbol("▶ ");

    f.render_stateful_widget(table, area, table_state);
}

fn participant_row<'a>(p: &'a Participant) -> Row<'a> {
    let stale = p.is_stale();

    // Status indicators
    let video = if p.video_enabled {
        Span::styled("[V]", Style::default().fg(Color::Cyan))
    } else {
        Span::styled("[ ]", Style::default().fg(Color::DarkGray))
    };
    let mic = if p.audio_enabled {
        Span::styled("[M]", Style::default().fg(Color::Green))
    } else {
        Span::styled("[ ]", Style::default().fg(Color::DarkGray))
    };
    // Note: Removed talking indicator - can't reliably detect from packet stream
    // Audio packets arrive at constant rate regardless of voice activity
    let status_line = Line::from(vec![video, mic]);

    // WT indicator
    let wt_indicator = if p.active_transport.as_deref() == Some("webtransport") {
        "✓"
    } else {
        " "
    };

    // RTT badge
    let (rtt_str, rtt_color) = match p.rtt_ms {
        Some(r) if r < 80.0 => (format!("{:>5}ms", r as u32), Color::Green),
        Some(r) if r < 150.0 => (format!("{:>5}ms", r as u32), Color::Yellow),
        Some(r) => (format!("{:>5}ms", r as u32), Color::Red),
        None => ("    --  ".to_string(), Color::DarkGray),
    };

    // Concealment — stale when tab throttled or no recent health packets
    let (conceal_str, conceal_color) = match &p.quality {
        Some(q) => {
            let stale = p.is_tab_throttled || q.updated_at.elapsed().as_secs() > 10;
            if stale {
                ("    --".to_string(), Color::DarkGray)
            } else {
                conceal_display(q.conceal_per_sec, q.audio_packets_per_sec)
            }
        }
        None => ("    --".to_string(), Color::DarkGray),
    };

    // Quality bar (10 chars wide) — driven by call_quality_score
    let (bar, bar_color) = quality_bar(p);

    // FPS and bitrate
    let (fps_str, kbps_str) = match &p.quality {
        Some(q) if p.video_enabled => (
            format!("{:>3}", q.fps as u32),
            format!("{:>5}", q.bitrate_kbps),
        ),
        _ => ("  --".to_string(), "   --".to_string()),
    };

    // Quality scores: prefer client-computed scores from HEALTH packets.
    // Format as right-aligned integer in a 3-char field; "--" when absent.
    let fmt_score = |s: Option<f64>| -> (String, Color) {
        match s {
            None => (" --".to_string(), Color::DarkGray),
            Some(v) => {
                let c = if v >= 75.0 {
                    Color::Green
                } else if v >= 40.0 {
                    Color::Yellow
                } else {
                    Color::Red
                };
                (format!("{:>3.0}", v), c)
            }
        }
    };
    let (aud_str, aud_color) = fmt_score(p.quality.as_ref().and_then(|q| q.audio_quality_score));
    let (vid_str, vid_color) = fmt_score(p.quality.as_ref().and_then(|q| q.video_quality_score));
    let (call_str, call_color) = fmt_score(p.quality.as_ref().and_then(|q| q.call_quality_score));

    // Name color-coding driven by call quality score
    let name_color = if stale { Color::DarkGray } else { call_color };

    let name_style = if stale {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::DIM)
    } else {
        Style::default().fg(name_color)
    };

    // Display: display_name (or user_id fallback) | user_id | s:last4
    let dn = p.display_name.as_deref().unwrap_or(&p.user_id);
    let last4 = &p.session_id[p.session_id.len().saturating_sub(4)..];
    let name_col = format!(
        "{:<20} | {:<20} | s:{}",
        truncate(dn, 20),
        truncate(&p.user_id, 20),
        last4
    );

    Row::new(vec![
        Cell::from(name_col).style(name_style),
        Cell::from(status_line),
        Cell::from(wt_indicator).style(Style::default().fg(Color::Cyan)),
        Cell::from(rtt_str).style(Style::default().fg(rtt_color)),
        Cell::from(conceal_str).style(Style::default().fg(conceal_color)),
        Cell::from(fps_str).style(Style::default().fg(if p.video_enabled {
            Color::Reset
        } else {
            Color::DarkGray
        })),
        Cell::from(kbps_str).style(Style::default().fg(if p.video_enabled {
            Color::Reset
        } else {
            Color::DarkGray
        })),
        Cell::from(aud_str).style(Style::default().fg(aud_color)),
        Cell::from(vid_str).style(Style::default().fg(vid_color)),
        Cell::from(call_str).style(Style::default().fg(call_color)),
        Cell::from(bar).style(Style::default().fg(bar_color)),
    ])
    .height(1)
}

fn quality_bar(p: &Participant) -> (String, Color) {
    // Prefer client-computed call_quality_score (0-100, higher = better).
    // Fall back to locally-computed score (0.0-1.0, lower = better) if not yet available.
    let (score_0_to_1, color) =
        if let Some(q) = p.quality.as_ref().and_then(|q| q.call_quality_score) {
            let s = q / 100.0; // convert 0-100 → 0.0-1.0 (higher = better)
            let c = if s >= 0.75 {
                Color::Green
            } else if s >= 0.4 {
                Color::Yellow
            } else {
                Color::Red
            };
            (s, c)
        } else {
            match p.quality_score() {
                // 0.0 = perfect, 1.0 = terrible
                None => return ("▒▒▒▒▒▒▒▒▒▒".to_string(), Color::DarkGray),
                Some(s) => {
                    let c = if s < 0.25 {
                        Color::Green
                    } else if s < 0.6 {
                        Color::Yellow
                    } else {
                        Color::Red
                    };
                    (1.0 - s, c) // invert so 1.0 = perfect for bar fill
                }
            }
        };
    let filled = (score_0_to_1 * 10.0) as usize;
    let filled = filled.min(10);
    let empty = 10 - filled;
    (
        format!("{}{}", "█".repeat(filled), "░".repeat(empty)),
        color,
    )
}

// ── Detail panel ──────────────────────────────────────────────────────────────

fn render_detail(
    f: &mut Frame,
    area: Rect,
    participants: &[&Participant],
    selected: Option<usize>,
) {
    let p = match selected.and_then(|i| participants.get(i)) {
        Some(p) => p,
        None => return,
    };

    let title = format!(
        " Detail: {} | {} | s:{} ",
        p.display_name.as_deref().unwrap_or(&p.user_id),
        p.user_id,
        p.session_id
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Blue))
        .title(Span::styled(title, Style::default().fg(Color::Cyan)));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rtt_str = p
        .rtt_ms
        .map(|r| format!("{}ms", r as u32))
        .unwrap_or_else(|| "--".to_string());

    let transport_str = p.active_transport.as_deref().unwrap_or("unknown");

    let mut lines: Vec<Line> = vec![Line::from(vec![
        label("Video: "),
        flag(p.video_enabled),
        Span::raw("   "),
        label("Audio: "),
        flag(p.audio_enabled),
        Span::raw("   "),
        label("Transport: "),
        Span::styled(transport_str, Style::default().fg(Color::Cyan)),
        Span::raw("   "),
        label("RTT: "),
        Span::styled(rtt_str, rtt_style_from(p.rtt_ms)),
        Span::raw("   "),
        label("Tab: "),
        if p.is_tab_visible {
            Span::styled("Visible", Style::default().fg(Color::Green))
        } else if p.is_tab_throttled {
            Span::styled("Throttled", Style::default().fg(Color::Red))
        } else {
            Span::styled("Hidden", Style::default().fg(Color::Yellow))
        },
    ])];

    // Add encoding and packet rate metrics
    let mut perf_line = vec![];
    if let Some(encode_ms) = p.avg_encode_latency_ms {
        perf_line.push(label("Encode: "));
        perf_line.push(colored_value(
            format!("{:.1}ms", encode_ms),
            if encode_ms < 10.0 {
                Color::Green
            } else if encode_ms < 30.0 {
                Color::Yellow
            } else {
                Color::Red
            },
        ));
        perf_line.push(Span::raw("   "));
    }
    if let Some(queue_bytes) = p.send_queue_bytes {
        perf_line.push(label("Queue: "));
        let kb = queue_bytes / 1024;
        perf_line.push(colored_value(
            format!("{}KB", kb),
            if kb < 100 {
                Color::Green
            } else if kb < 500 {
                Color::Yellow
            } else {
                Color::Red
            },
        ));
        perf_line.push(Span::raw("   "));
    }
    if let Some(rx_rate) = p.packets_received_per_sec {
        perf_line.push(label("Rx: "));
        perf_line.push(Span::styled(
            format!("{:.0}pkt/s", rx_rate),
            Style::default().fg(Color::Reset),
        ));
        perf_line.push(Span::raw("   "));
    }
    if let Some(tx_rate) = p.packets_sent_per_sec {
        perf_line.push(label("Tx: "));
        perf_line.push(Span::styled(
            format!("{:.0}pkt/s", tx_rate),
            Style::default().fg(Color::Reset),
        ));
        perf_line.push(Span::raw("   "));
    }
    if let Some(mem_bytes) = p.memory_used_bytes {
        perf_line.push(label("Mem: "));
        let mb = mem_bytes / (1024 * 1024);
        perf_line.push(Span::styled(
            format!("{}MB", mb),
            Style::default().fg(Color::Reset),
        ));
    }
    if !perf_line.is_empty() {
        lines.push(Line::from(perf_line));
    }

    match &p.quality {
        None => {
            lines.push(Line::from(Span::styled(
                "  No quality data received yet (waiting for HEALTH packets)",
                Style::default().fg(Color::DarkGray),
            )));
        }
        Some(q) => {
            let age_secs = q.updated_at.elapsed().as_secs();
            let stale = p.is_tab_throttled || age_secs > 10;
            let stale_label = if p.is_tab_throttled {
                " (throttled)"
            } else {
                " (stale)"
            };
            let no_audio = q.audio_packets_per_sec < 2.0;
            // Line 1: Audio jitter — real network jitter (target_delay_ms) + buffer depth
            lines.push(Line::from(vec![
                label("Jitter: "),
                if stale {
                    colored_value(format!("--{}", stale_label), Color::DarkGray)
                } else if no_audio {
                    colored_value("-- (no audio)".to_string(), Color::DarkGray)
                } else {
                    colored_value(
                        format!("{}ms", q.target_delay_ms as u32),
                        jitter_color(q.target_delay_ms),
                    )
                },
                Span::raw("  "),
                label("Buf: "),
                if stale || no_audio {
                    colored_value("--".to_string(), Color::DarkGray)
                } else {
                    colored_value(format!("{}ms", q.buf_depth_ms as u32), Color::DarkGray)
                },
                Span::raw("   "),
                label("Conceal: "),
                {
                    let (cs, cc) = conceal_display(q.conceal_per_sec, q.audio_packets_per_sec);
                    colored_value(cs, cc)
                },
                Span::raw("   "),
                label("Pkt Loss: "),
                colored_value(
                    format!("{:.1}%", q.audio_packet_loss_pct),
                    if q.audio_packet_loss_pct < 1.0 {
                        Color::Green
                    } else if q.audio_packet_loss_pct < 5.0 {
                        Color::Yellow
                    } else {
                        Color::Red
                    },
                ),
            ]));
            // Line 2: Video metrics
            let mut video_line = vec![
                label("FPS: "),
                Span::styled(
                    format!("{}", q.fps as u32),
                    Style::default().fg(Color::Reset),
                ),
                Span::raw("   "),
                label("Bitrate: "),
                Span::styled(
                    format!("{} kbps", q.bitrate_kbps),
                    Style::default().fg(Color::Reset),
                ),
                Span::raw("   "),
                label("DecErr: "),
                colored_value(
                    format!("{:.1}/s", q.decode_errors_per_sec),
                    if q.decode_errors_per_sec < 0.5 {
                        Color::Green
                    } else if q.decode_errors_per_sec < 5.0 {
                        Color::Yellow
                    } else {
                        Color::Red
                    },
                ),
            ];
            if let Some(decode_ms) = q.avg_decode_latency_ms {
                video_line.push(Span::raw("   "));
                video_line.push(label("Decode: "));
                video_line.push(colored_value(
                    format!("{:.1}ms", decode_ms),
                    if decode_ms < 10.0 {
                        Color::Green
                    } else if decode_ms < 30.0 {
                        Color::Yellow
                    } else {
                        Color::Red
                    },
                ));
            }
            lines.push(Line::from(video_line));
            // Line 3: Quality scores (client-computed, absent when stream inactive)
            let has_score = q.audio_quality_score.is_some() || q.video_quality_score.is_some();
            if has_score {
                let mut score_line = vec![label("Score: ")];
                if let Some(a) = q.audio_quality_score {
                    score_line.push(label("Audio "));
                    score_line.push(colored_value(
                        format!("{:.0}", a),
                        if a >= 75.0 {
                            Color::Green
                        } else if a >= 40.0 {
                            Color::Yellow
                        } else {
                            Color::Red
                        },
                    ));
                }
                if let Some(v) = q.video_quality_score {
                    score_line.push(Span::raw("   "));
                    score_line.push(label("Video "));
                    score_line.push(colored_value(
                        format!("{:.0}", v),
                        if v >= 75.0 {
                            Color::Green
                        } else if v >= 40.0 {
                            Color::Yellow
                        } else {
                            Color::Red
                        },
                    ));
                }
                if let Some(c) = q.call_quality_score {
                    score_line.push(Span::raw("   "));
                    score_line.push(label("Call "));
                    score_line.push(colored_value(
                        format!("{:.0}", c),
                        if c >= 75.0 {
                            Color::Green
                        } else if c >= 40.0 {
                            Color::Yellow
                        } else {
                            Color::Red
                        },
                    ));
                }
                lines.push(Line::from(score_line));
            }
            // Data age indicator
            if age_secs > 5 {
                lines.push(Line::from(Span::styled(
                    format!("  (data {}s old)", age_secs),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            // Quality interpretation
            let interpretation = interpret_quality(q, p.rtt_ms);
            if !interpretation.is_empty() {
                lines.push(Line::from(vec![
                    Span::styled("  ⚠ ", Style::default().fg(Color::Yellow)),
                    Span::styled(interpretation, Style::default().fg(Color::Yellow)),
                ]));
            }
        }
    }

    let paragraph = Paragraph::new(lines);
    f.render_widget(paragraph, inner);
}

fn interpret_quality(q: &QualitySnapshot, rtt_ms: Option<f64>) -> String {
    let mut issues = Vec::new();
    if q.conceal_per_sec > 5.0 && q.audio_packets_per_sec > 2.0 {
        issues.push(format!(
            "high audio concealment ({:.1}/s)",
            q.conceal_per_sec
        ));
    }
    if q.target_delay_ms > 75.0 && q.audio_packets_per_sec >= 2.0 {
        issues.push(format!(
            "high jitter ({}ms target delay)",
            q.target_delay_ms as u32
        ));
    }
    if let Some(rtt) = rtt_ms {
        if rtt > 150.0 {
            issues.push(format!("high RTT ({}ms)", rtt as u32));
        }
    }
    issues.join(", ")
}

// ── Events panel ─────────────────────────────────────────────────────────────

fn render_events(
    f: &mut Frame,
    area: Rect,
    state: &MeetingState,
    use_utc: bool,
    focus: Focus,
    event_scroll: usize,
) {
    let focused = focus == Focus::Events;
    let border_style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let total = state.events.len();
    let scroll_indicator = if event_scroll > 0 {
        format!(" Events ↑{} ", event_scroll)
    } else {
        " Events ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(scroll_indicator);
    let inner = block.inner(area);
    f.render_widget(block, area);

    let max_lines = inner.height as usize;
    // event_scroll=0 means newest at bottom; event_scroll=N means scroll back N lines
    let end = total.saturating_sub(event_scroll);
    let start = end.saturating_sub(max_lines);

    let lines: Vec<Line> = state
        .events
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|ev| {
            let when_str = if use_utc {
                ev.when
                    .with_timezone(&Utc)
                    .format("%H:%M:%S UTC")
                    .to_string()
            } else {
                ev.when.format("%H:%M:%S").to_string()
            };
            Line::from(vec![
                Span::styled(when_str, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::raw(ev.msg.clone()),
            ])
        })
        .collect();

    // If there's history above the visible window, show a hint on the first line
    let mut display_lines = lines;
    if start > 0 && !display_lines.is_empty() {
        display_lines.insert(
            0,
            Line::from(Span::styled(
                format!("  ↑ {} more", start),
                Style::default().fg(Color::DarkGray),
            )),
        );
        display_lines.truncate(max_lines);
    }

    f.render_widget(Paragraph::new(display_lines), inner);
}

// ── Footer ────────────────────────────────────────────────────────────────────

fn render_footer(f: &mut Frame, area: Rect, focus: Focus) {
    let focus_label = match focus {
        Focus::Participants => "participants",
        Focus::Events => "events",
    };
    let line = Line::from(vec![
        Span::styled(" [h/?]", Style::default().fg(Color::Yellow)),
        Span::raw(" help  "),
        Span::styled("[q]", Style::default().fg(Color::Yellow)),
        Span::raw(" quit  "),
        Span::styled("[Tab]", Style::default().fg(Color::Yellow)),
        Span::raw(format!(" focus: {}  ", focus_label)),
        Span::styled("[↑/↓] [j/k]", Style::default().fg(Color::Yellow)),
        Span::raw(" navigate"),
        Span::styled(
            "                        vcprobe 0.1",
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ── Help Screen ───────────────────────────────────────────────────────────────

fn render_help(f: &mut Frame, area: Rect) {
    // Calculate centered position
    let width = area.width.min(82);
    let height = area.height.min(52);
    let x = (area.width.saturating_sub(width)) / 2;
    let y = (area.height.saturating_sub(height)) / 2;

    let help_area = Rect {
        x: area.x + x,
        y: area.y + y,
        width,
        height,
    };

    let help_text = vec![
        Line::from(Span::styled(
            "vcprobe - Proctor Mode Help",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "Keyboard Controls:",
            Style::default().fg(Color::Yellow),
        )),
        Line::from("  h, ?      Show/hide this help screen"),
        Line::from("  s, S      Toggle sort: Name ↔ Call Quality (worst first)"),
        Line::from("  q, Esc    Quit proctor mode"),
        Line::from("  Tab       Switch focus: Participants ↔ Events"),
        Line::from("  ↑/↓, j/k  Navigate participants or scroll events"),
        Line::from("  Home/End  Jump to first/last"),
        Line::from(""),
        Line::from(Span::styled(
            "Main Table Columns:",
            Style::default().fg(Color::Yellow),
        )),
        Line::from("  [V][M]  Camera and microphone enabled (sender self-report)"),
        Line::from(""),
        Line::from(
            "  WT      WebTransport indicator. ✓ if using WebTransport, blank if WebSocket.",
        ),
        Line::from(""),
        Line::from(
            "  RTT     Round-trip time to the media relay server (self-reported by client).",
        ),
        Line::from("          Reflects one leg of the end-to-end path. True conversational delay"),
        Line::from("          is approximately RTT_you + RTT_them. ITU-T G.114: <150ms one-way"),
        Line::from("          (~300ms RTT) for natural conversation."),
        Line::from("          Green: <80ms  Yellow: 80-150ms  Red: >150ms"),
        Line::from(""),
        Line::from("  Conc/s  Audio concealment rate — the gold standard audio quality signal."),
        Line::from("          NetEQ fires an 'expand' operation whenever the jitter buffer runs"),
        Line::from("          dry (packet lost, arrived too late, or DTX gap). The browser"),
        Line::from("          synthesizes audio to cover the gap. This is what callers hear as"),
        Line::from("          choppiness, robotic artifacts, or dropouts."),
        Line::from("          0.0/s = perfect   1-5/s = noticeable   >5/s = degraded"),
        Line::from("          Green: <1/s  Yellow: 1-5/s  Red: >5/s"),
        Line::from(""),
        Line::from("  FPS     Video frames decoded per second (observed by remote peers)."),
        Line::from("          Primary video quality indicator. <15fps is visually degraded;"),
        Line::from("          <10fps is poor. Low FPS with normal bitrate = decode bottleneck."),
        Line::from(""),
        Line::from("  kbps    Video bitrate in kbits/sec (observed by remote peers)."),
        Line::from("          Context for FPS: low kbps + good FPS = compressed but smooth."),
        Line::from("          Low kbps + low FPS = real quality problem."),
        Line::from(""),
        Line::from("  Aud     Audio quality score 0-100 (computed by the observing client)."),
        Line::from("          Formula: 100 - concealment_penalty(max 70) - loss_penalty(max 30)"),
        Line::from("          Concealment dominates because it directly causes audible artifacts."),
        Line::from("          Absent (--) when audio is inactive or data is stale (>5s)."),
        Line::from("          Green: ≥75  Yellow: 40-74  Red: <40"),
        Line::from(""),
        Line::from("  Vid     Video quality score 0-100 (video health, not FPS quality)."),
        Line::from("          Formula: health(fps>=5→100, fps<5→0–50) - decode_error_penalty"),
        Line::from("          Absent (--) when video is inactive or data is stale (>5s)."),
        Line::from("          Green: ≥75  Yellow: 40-74  Red: <40"),
        Line::from(""),
        Line::from("  Call    Call quality score 0-100 = min(Aud, Vid)."),
        Line::from("          Worst stream determines the call experience. This is the primary"),
        Line::from("          single-number summary for a participant's call health."),
        Line::from("          Name color and sort order are both driven by this score."),
        Line::from("          Green: ≥75  Yellow: 40-74  Red: <40"),
        Line::from(""),
        Line::from("  Bar     Visual representation of Call quality score (10-char block fill)."),
        Line::from(""),
        Line::from(Span::styled(
            "Detail Panel  (select a row with ↑/↓):",
            Style::default().fg(Color::Yellow),
        )),
        Line::from("  Jitter    NetEQ target delay ms. In this stack often settles at a fixed"),
        Line::from("            default (~120ms); Conc/s is the more reliable audio indicator."),
        Line::from("  Buf       Current jitter buffer depth (ms)."),
        Line::from("  Pkt Loss  Audio packet loss % (expand_per_sec / packets_per_sec proxy)."),
        Line::from("            Green: <1%  Yellow: 1-5%  Red: >5%"),
        Line::from("  DecErr    Codec decode errors/sec (keyframe miss, parser error, reset)."),
        Line::from("            NOT the same as CPU-pressure frame drops. Usually near 0."),
        Line::from("  Encode    Average encode latency (ms). >30ms indicates CPU pressure."),
        Line::from("  Decode    Average decode latency (ms). >30ms indicates decode bottleneck."),
        Line::from("  Queue     WebSocket send queue (KB). >100KB = send-side backpressure."),
        Line::from("  Rx/Tx     Packet rates (packets/sec) at the transport layer."),
        Line::from("  Mem       JavaScript heap usage (MB). Chrome only; blank on Firefox/Safari."),
        Line::from("  Tab       Browser tab state: Visible / Hidden / Throttled."),
        Line::from(""),
        Line::from(Span::styled("Notes:", Style::default().fg(Color::Yellow))),
        Line::from("  • Aud/Vid/Call scores are observer-reported: each participant computes"),
        Line::from("    quality scores for the peers they receive, not for themselves."),
        Line::from("  • Quality data requires HEALTH packets from at least one other participant."),
        Line::from("  • Solo users show RTT but no Aud/Vid/Call scores or FPS/kbps data."),
        Line::from("  • Rows turn gray after ~2s without heartbeat packets (stale/disconnected)."),
        Line::from(""),
        Line::from(Span::styled(
            "Press h or ? to close",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let block = Block::default()
        .title(" Help ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(Color::Black));

    let paragraph = Paragraph::new(help_text).block(block);
    f.render_widget(paragraph, help_area);
}

// ── Formatting helpers ────────────────────────────────────────────────────────

fn label(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(Color::DarkGray))
}

fn flag(on: bool) -> Span<'static> {
    if on {
        Span::styled("✓", Style::default().fg(Color::Green))
    } else {
        Span::styled("✗", Style::default().fg(Color::DarkGray))
    }
}

fn colored_value(s: String, color: Color) -> Span<'static> {
    Span::styled(s, Style::default().fg(color))
}

fn rtt_style_from(rtt: Option<f64>) -> Style {
    match rtt {
        Some(r) if r < 80.0 => Style::default().fg(Color::Green),
        Some(r) if r < 150.0 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Red),
        None => Style::default().fg(Color::DarkGray),
    }
}

/// Color for the real jitter metric (target_delay_ms from delay manager).
/// VoIP guideline: <30ms excellent, <75ms acceptable, >=75ms degraded.
fn jitter_color(target_delay_ms: f64) -> Color {
    if target_delay_ms < 30.0 {
        Color::Green
    } else if target_delay_ms < 75.0 {
        Color::Yellow
    } else {
        Color::Red
    }
}

/// Returns (display_string, color) for the concealment/s column.
/// When packets_per_sec ≈ 0 and concealment is high, the speaker is simply silent (DTX) —
/// not actually losing packets. Show "silent" in DarkGray to avoid false alarms.
fn conceal_display(cps: f64, pkts_per_sec: f64) -> (String, Color) {
    if pkts_per_sec < 2.0 && cps > 5.0 {
        ("silent".to_string(), Color::DarkGray)
    } else {
        let color = if cps < 1.0 {
            Color::Green
        } else if cps < 5.0 {
            Color::Yellow
        } else {
            Color::Red
        };
        (format!("{:>5.1}/s", cps), color)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max - 1])
    }
}
