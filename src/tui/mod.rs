mod browser;
mod engine;
mod view;

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

use crate::output::stream::release_claimed_device;
use crate::output::usb::device::release_claimed_dac;
use crate::player::{PlayOptions, Target};
use crate::tui::browser::Browser;
use crate::tui::engine::Engine;

/// How long a redraw waits for a key before refreshing the counters anyway.
const TICK: Duration = Duration::from_millis(100);

pub struct App {
    browser: Browser,
    engine: Engine,
    quit: bool,
}

/// Run the browser and transport until the user quits, restoring the terminal on the way out.
pub fn run(dir: PathBuf, device: Option<&str>, options: PlayOptions) -> Result<()> {
    let target = Target::resolve(device)?;
    let dir = dir
        .canonicalize()
        .with_context(|| format!("cannot open {}", dir.display()))?;
    let mut app = App {
        browser: Browser::open(dir),
        engine: Engine::spawn(target, options),
        quit: false,
    };

    let mut terminal = enter_terminal()?;
    let result = app.event_loop(&mut terminal);
    leave_terminal();
    result
}

impl App {
    fn event_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        while !self.quit {
            let status = self.engine.status();
            terminal.draw(|frame| view::draw(frame, &self.browser, &status))?;
            if !event::poll(TICK)? {
                continue;
            }
            if let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
            {
                self.on_key(key);
            }
        }
        Ok(())
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Up | KeyCode::Char('k') => self.browser.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.browser.move_by(1),
            KeyCode::PageUp => self.browser.move_by(-10),
            KeyCode::PageDown => self.browser.move_by(10),
            KeyCode::Home => self.browser.move_to(0),
            KeyCode::End => self.browser.move_to(usize::MAX),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_selection(),
            KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => self.go_up(),
            KeyCode::Char(' ') => self.engine.toggle(),
            KeyCode::Char('s') => self.engine.stop(),
            KeyCode::Char('n') => self.engine.skip(1),
            KeyCode::Char('p') => self.engine.skip(-1),
            KeyCode::Char('r') => self.browser.refresh(),
            _ => {}
        }
    }

    /// A folder is descended into; a file starts the whole folder playing from that file.
    fn open_selection(&mut self) {
        let Some(entry) = self.browser.selection() else {
            return;
        };
        if entry.is_dir {
            let dir = entry.path.clone();
            self.browser.enter(dir);
            return;
        }
        let path = entry.path.clone();
        let files = self.browser.playable();
        let index = files.iter().position(|file| *file == path).unwrap_or(0);
        self.engine.load(files, index);
    }

    fn go_up(&mut self) {
        let Some(parent) = self.browser.dir.parent().map(PathBuf::from) else {
            return;
        };
        self.browser.enter(parent);
    }
}

fn enter_terminal() -> Result<ratatui::DefaultTerminal> {
    // The device is claimed on the worker thread, so a signal that kills this process has to
    // hand it back itself - and put the terminal back, or the shell is left in raw mode. A
    // DAC held for native DSD needs re-enumerating too, or it stays missing from Core Audio
    // until it is physically replugged.
    ctrlc::set_handler(|| {
        leave_terminal();
        release_claimed_device();
        release_claimed_dac();
        std::process::exit(130);
    })?;
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        leave_terminal();
        previous(info);
    }));

    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(io::stdout()),
    )?)
}

fn leave_terminal() {
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
    let _ = disable_raw_mode();
}
