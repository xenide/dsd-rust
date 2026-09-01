use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Gauge, List, ListItem, ListState, Paragraph};

use crate::player::{DeviceInfo, Progress, TrackInfo, clock};
use crate::tui::browser::Browser;
use crate::tui::engine::{State, Status};

const ACCENT: Color = Color::Cyan;
const KEYS: &str =
    " ↑↓ move   enter open   space play/pause   s stop   n/p track   r refresh   q quit";

pub fn draw(frame: &mut Frame, browser: &Browser, status: &Status) {
    let [top, debug, hints] = Layout::vertical([
        Constraint::Min(6),
        Constraint::Length(10),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [files, playing] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);

    draw_files(frame, files, browser, status);
    draw_playing(frame, playing, status);
    draw_debug(frame, debug, status);
    frame.render_widget(
        Paragraph::new(footer(browser, status)).style(Style::new().fg(Color::DarkGray)),
        hints,
    );
}

fn draw_files(frame: &mut Frame, area: Rect, browser: &Browser, status: &Status) {
    let title = format!(" {} ", short_path(&browser.dir));
    let mut items = Vec::new();
    for entry in &browser.entries {
        let playing = status.path.as_deref() == Some(entry.path.as_path());
        let (marker, style) = if playing {
            ("▸ ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD))
        } else if entry.is_dir {
            ("  ", Style::new().fg(Color::Blue))
        } else {
            ("  ", Style::new())
        };
        let suffix = if entry.is_dir { "/" } else { "" };
        items.push(ListItem::new(Line::from(vec![
            Span::raw(marker),
            Span::styled(format!("{}{suffix}", entry.name), style),
        ])));
    }

    let mut state = ListState::default().with_selected(Some(browser.selected));
    frame.render_stateful_widget(
        List::new(items)
            .block(Block::bordered().title(title))
            .highlight_style(Style::new().bg(Color::DarkGray).fg(Color::White)),
        area,
        &mut state,
    );
}

fn draw_playing(frame: &mut Frame, area: Rect, status: &Status) {
    let block = Block::bordered().title(" now playing ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 4 {
        return;
    }

    let [header, bar, _] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    let name = status
        .path
        .as_ref()
        .and_then(|path| path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "nothing loaded".to_owned());
    let format = match &status.track {
        Some(track) => format!("{}  {}", track.format, track.container),
        None => "—".to_owned(),
    };
    let elapsed = status.progress.elapsed;
    let duration = status.track.as_ref().map_or(0.0, |track| track.duration);

    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(name, Style::new().add_modifier(Modifier::BOLD)),
            Line::styled(format, Style::new().fg(Color::DarkGray)),
            Line::from(vec![
                Span::styled(status.state.glyph(), Style::new().fg(ACCENT)),
                Span::raw(format!(
                    " {}   {} / {}{}",
                    status.state.label(),
                    clock(elapsed),
                    clock(duration),
                    match status.playlist.len() {
                        0 => String::new(),
                        total => format!("   track {} of {total}", status.index + 1),
                    }
                )),
            ]),
        ]),
        header,
    );

    let ratio = if duration > 0.0 {
        (elapsed / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    frame.render_widget(
        Gauge::default()
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Black))
            .ratio(ratio)
            .label(format!("{:.0}%", ratio * 100.0)),
        bar,
    );
}

fn draw_debug(frame: &mut Frame, area: Rect, status: &Status) {
    let mut rows = match (&status.device, &status.track) {
        (Some(device), Some(track)) => debug_rows(device, track, status.progress),
        _ => vec![Line::styled(
            "nothing playing",
            Style::new().fg(Color::DarkGray),
        )],
    };
    if let Some(error) = &status.error {
        rows.push(Line::styled(
            format!("error      {error}"),
            Style::new().fg(Color::Red),
        ));
    }

    frame.render_widget(
        Paragraph::new(rows).block(Block::bordered().title(" debug ")),
        area,
    );
}

fn debug_rows(device: &DeviceInfo, track: &TrackInfo, progress: Progress) -> Vec<Line<'static>> {
    let rate = f64::from(device.pcm_rate);
    let claim = if device.exclusive {
        "exclusive"
    } else {
        "shared"
    };
    let mixing = if device.mixing_disabled {
        ", mixing off"
    } else {
        ""
    };
    let volume = match device.volume {
        Some(volume) if volume < 1.0 => {
            format!(
                ", device volume {:.0}% — may break DSD lock",
                volume * 100.0
            )
        }
        _ => String::new(),
    };
    vec![
        row("device", format!("{}  {claim}{mixing}", device.name)),
        row(
            "transport",
            format!(
                "{} {} bit, {} {} Hz{volume}",
                device.transport, device.bits, device.carrier, device.pcm_rate
            ),
        ),
        row(
            "io buffer",
            format!(
                "{} frames ({:.1} ms)",
                device.buffer_frames,
                f64::from(device.buffer_frames) * 1000.0 / rate
            ),
        ),
        row(
            "queue",
            format!(
                "{:>3.0}%   {} of {} frames",
                progress.queue_fill() * 100.0,
                progress.queued_frames,
                progress.queue_frames
            ),
        ),
        row(
            "underruns",
            format!(
                "{} frames ({:.0} ms of DSD silence)",
                progress.underrun_frames,
                progress.underrun_frames as f64 * 1000.0 / rate
            ),
        ),
        row(
            "frames",
            format!("{} of {}", progress.frames_played, track.total_frames),
        ),
        row(
            "dsd",
            format!(
                "{} bit/s per channel, {} bytes per channel",
                track.format.rate.hz(),
                track.bytes_per_channel
            ),
        ),
    ]
}

/// Directory titles are long and the pane is narrow, so abbreviate home and keep the tail.
fn short_path(dir: &std::path::Path) -> String {
    let mut text = dir.display().to_string();
    if let Some(home) = std::env::var_os("HOME")
        && let Some(rest) = text.strip_prefix(&home.to_string_lossy().into_owned())
    {
        text = format!("~{rest}");
    }
    text
}

fn row(label: &str, value: String) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{label:<11}"), Style::new().fg(Color::DarkGray)),
        Span::raw(value),
    ])
}

/// The footer replaces the key hints with whatever most recently went wrong.
fn footer(browser: &Browser, status: &Status) -> String {
    if let Some(error) = &browser.error {
        return format!(" {error}");
    }
    match &status.error {
        Some(error) if status.state == State::Stopped => format!(" {error}"),
        _ => KEYS.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use crate::dsd::{DsdFormat, DsdRate};
    use crate::player::{DeviceInfo, Progress, TrackInfo};
    use crate::tui::browser::Browser;
    use crate::tui::engine::{State, Status};
    use crate::tui::view::draw;

    fn playing_status(dir: &std::path::Path) -> Status {
        Status {
            state: State::Playing,
            path: Some(dir.join("track.dsf")),
            track: Some(TrackInfo {
                container: "DSF",
                format: DsdFormat {
                    rate: DsdRate::new(5_644_800),
                    channels: 2,
                },
                duration: 120.0,
                total_frames: 42_336_000,
                bytes_per_channel: 84_672_000,
            }),
            device: Some(DeviceInfo {
                carrier: "DoP",
                bits: 24,
                name: "Topping D50".to_owned(),
                pcm_rate: 352_800,
                buffer_frames: 512,
                transport: "integer",
                exclusive: true,
                mixing_disabled: true,
                volume: None,
            }),
            progress: Progress {
                elapsed: 30.0,
                frames_played: 10_584_000,
                underrun_frames: 0,
                queued_frames: 44_100,
                queue_frames: 88_200,
            },
            playlist: vec![dir.join("track.dsf")],
            index: 0,
            error: None,
        }
    }

    fn render(browser: &Browser, status: &Status) -> String {
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).expect("test terminal");
        terminal
            .draw(|frame| draw(frame, browser, status))
            .expect("draws");
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        text
    }

    #[test]
    fn a_playing_track_shows_its_transport_and_debug_counters() {
        let dir = tempfile::tempdir().expect("temp dir");
        std::fs::write(dir.path().join("track.dsf"), b"").expect("file");
        let browser = Browser::open(dir.path().to_path_buf());

        let screen = render(&browser, &playing_status(dir.path()));

        assert!(screen.contains("track.dsf"), "{screen}");
        assert!(screen.contains("DSD128"), "{screen}");
        assert!(screen.contains("playing   0:30 / 2:00"), "{screen}");
        assert!(
            screen.contains("Topping D50  exclusive, mixing off"),
            "{screen}"
        );
        assert!(screen.contains("integer 24 bit, DoP 352800 Hz"), "{screen}");
        assert!(screen.contains("512 frames (1.5 ms)"), "{screen}");
        assert!(screen.contains("50%"), "{screen}");
    }

    #[test]
    fn a_natively_streamed_track_names_its_carrier_and_container_width() {
        let dir = tempfile::tempdir().expect("temp dir");
        let mut status = playing_status(dir.path());
        let device = status.device.as_mut().expect("device");
        device.carrier = "native DSD";
        device.bits = 32;
        device.name = "Cayin RU7".to_owned();

        let browser = Browser::open(dir.path().to_path_buf());
        let screen = render(&browser, &status);

        assert!(
            screen.contains("integer 32 bit, native DSD 352800 Hz"),
            "{screen}"
        );
        assert!(!screen.contains("DoP"), "{screen}");
    }

    #[test]
    fn an_idle_screen_says_so_and_keeps_the_key_hints() {
        let dir = tempfile::tempdir().expect("temp dir");
        let browser = Browser::open(dir.path().to_path_buf());

        let screen = render(&browser, &Status::default());

        assert!(screen.contains("nothing loaded"), "{screen}");
        assert!(screen.contains("nothing playing"), "{screen}");
        assert!(screen.contains("space play/pause"), "{screen}");
    }

    #[test]
    fn a_failed_load_replaces_the_key_hints_with_the_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let browser = Browser::open(dir.path().to_path_buf());
        let status = Status {
            error: Some("device is busy".to_owned()),
            ..Status::default()
        };

        let screen = render(&browser, &status);

        assert!(screen.contains("device is busy"), "{screen}");
        assert!(!screen.contains("space play/pause"), "{screen}");
    }
}
