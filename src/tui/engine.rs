use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::player::{DeviceInfo, PlayOptions, Playback, Progress, Target, TrackInfo};

const POLL: Duration = Duration::from_millis(20);
/// How long the worker waits for a command when there is nothing playing to watch.
const IDLE: Duration = Duration::from_millis(100);

#[derive(Debug)]
enum Command {
    Load { files: Vec<PathBuf>, index: usize },
    Toggle,
    Stop,
    Skip(i32),
    Quit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum State {
    #[default]
    Stopped,
    Playing,
    Paused,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Playing => "playing",
            Self::Paused => "paused",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Stopped => "■",
            Self::Playing => "▶",
            Self::Paused => "❙❙",
        }
    }
}

/// Everything the view draws, republished by the worker on every tick.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub state: State,
    pub path: Option<PathBuf>,
    pub track: Option<TrackInfo>,
    pub device: Option<DeviceInfo>,
    pub progress: Progress,
    pub playlist: Vec<PathBuf>,
    pub index: usize,
    pub error: Option<String>,
}

/// A playback worker on its own thread, so the audio path never waits on a redraw.
pub struct Engine {
    commands: Sender<Command>,
    status: Arc<Mutex<Status>>,
    worker: Option<thread::JoinHandle<()>>,
}

impl Engine {
    pub fn spawn(target: Target, options: PlayOptions) -> Self {
        let (commands, receiver) = channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let published = Arc::clone(&status);
        // The worker is built on its own thread because a live session owns either the Core
        // Audio callback context or the USB device, neither of which travels between threads.
        let worker = thread::spawn(move || {
            Worker {
                target,
                options,
                status: published,
                commands: receiver,
                session: None,
                stop: Arc::new(AtomicBool::new(false)),
                playlist: Vec::new(),
                index: 0,
            }
            .run();
        });
        Self {
            commands,
            status,
            worker: Some(worker),
        }
    }

    pub fn status(&self) -> Status {
        self.status
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn load(&self, files: Vec<PathBuf>, index: usize) {
        self.send(Command::Load { files, index });
    }

    pub fn toggle(&self) {
        self.send(Command::Toggle);
    }

    pub fn stop(&self) {
        self.send(Command::Stop);
    }

    pub fn skip(&self, delta: i32) {
        self.send(Command::Skip(delta));
    }

    fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.send(Command::Quit);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

struct Worker {
    target: Target,
    options: PlayOptions,
    status: Arc<Mutex<Status>>,
    commands: Receiver<Command>,
    session: Option<Playback>,
    stop: Arc<AtomicBool>,
    playlist: Vec<PathBuf>,
    index: usize,
}

impl Worker {
    fn run(mut self) {
        loop {
            let timeout = if self.session.is_some() { POLL } else { IDLE };
            match self.commands.recv_timeout(timeout) {
                Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
            }
            if !self.end_if_stalled() {
                self.advance_if_complete();
            }
            self.publish();
        }
        self.stop_playback();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Load { files, index } => {
                self.playlist = files;
                self.start(index);
            }
            Command::Toggle => match &self.session {
                Some(session) => session.set_paused(!session.is_paused()),
                None => self.start(self.index),
            },
            Command::Stop => self.stop_playback(),
            Command::Skip(delta) => {
                let next = self.index as i32 + delta;
                if next >= 0 && (next as usize) < self.playlist.len() {
                    self.start(next as usize);
                }
            }
            Command::Quit => {}
        }
    }

    /// Play the track at `index`, replacing whatever is playing now.
    fn start(&mut self, index: usize) {
        self.end_session();
        let Some(path) = self.playlist.get(index).cloned() else {
            return;
        };
        self.index = index;
        self.set_error(None);
        match Playback::open(&path, &mut self.target, &self.options, &self.stop) {
            Ok(session) => self.session = Some(session),
            Err(error) => self.set_error(Some(format!("{path:?}: {error:#}"))),
        }
    }

    /// Move to the next track once the current one has drained, and stop at the end.
    fn advance_if_complete(&mut self) {
        if !self.session.as_ref().is_some_and(Playback::is_complete) {
            return;
        }
        let next = self.index + 1;
        if next < self.playlist.len() {
            self.start(next);
        } else {
            self.stop_playback();
        }
    }

    /// A dead engine ends playback with a reason, rather than leaving the transport saying
    /// "playing" over a counter that never moves and a DAC nothing will hand back.
    fn end_if_stalled(&mut self) -> bool {
        if !self.session.as_ref().is_some_and(Playback::has_stalled) {
            return false;
        }
        self.stop_playback();
        self.set_error(Some(
            "the DAC stopped accepting transfers, so playback ended".to_owned(),
        ));
        true
    }

    /// End the current track, leaving a natively claimed DAC held for the next one.
    fn end_session(&mut self) {
        let Some(session) = self.session.take() else {
            return;
        };
        if let Err(error) = session.finish(&mut self.target) {
            self.set_error(Some(format!("{error:#}")));
        }
    }

    /// Stop playing altogether and give a natively claimed DAC back, so the rest of the
    /// system can use it again while nothing is playing.
    fn stop_playback(&mut self) {
        self.end_session();
        self.target.release_dac();
    }

    fn set_error(&self, error: Option<String>) {
        self.status
            .lock()
            .unwrap_or_else(|guard| guard.into_inner())
            .error = error;
    }

    fn publish(&self) {
        let mut status = self
            .status
            .lock()
            .unwrap_or_else(|guard| guard.into_inner());
        status.playlist = self.playlist.clone();
        status.index = self.index;
        let Some(session) = &self.session else {
            status.state = State::Stopped;
            status.progress = Progress::default();
            return;
        };
        status.state = if session.is_paused() {
            State::Paused
        } else {
            State::Playing
        };
        status.path = self.playlist.get(self.index).cloned();
        status.track = Some(session.track());
        status.device = Some(session.device());
        status.progress = session.progress();
    }
}
