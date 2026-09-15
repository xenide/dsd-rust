use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::reader::sacd::{self, Disc};
use crate::reader::{self, TrackRef};

/// What one row of the file pane stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Folder,
    /// A disc image. It opens like a folder, because it holds a list of tracks rather than
    /// one recording.
    Disc,
    Track(TrackRef),
}

impl Kind {
    /// True for a row the pane descends into rather than plays.
    pub const fn opens(&self) -> bool {
        match self {
            Self::Folder | Self::Disc => true,
            Self::Track(_) => false,
        }
    }

    pub const fn track(&self) -> Option<&TrackRef> {
        match self {
            Self::Track(track) => Some(track),
            Self::Folder | Self::Disc => None,
        }
    }
}

/// One row of the file pane.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// What the recording's tags call it, when it carries any.
    pub title: Option<String>,
    pub path: PathBuf,
    pub kind: Kind,
}

/// A cursor over one folder, or over one disc image, showing only what this player can open.
pub struct Browser {
    pub dir: PathBuf,
    pub entries: Vec<Entry>,
    pub selected: usize,
    pub error: Option<String>,
}

impl Browser {
    pub fn open(dir: PathBuf) -> Self {
        let mut browser = Self {
            dir,
            entries: Vec::new(),
            selected: 0,
            error: None,
        };
        browser.refresh();
        browser
    }

    pub fn refresh(&mut self) {
        match list(&self.dir) {
            Ok(entries) => {
                self.entries = entries;
                self.error = None;
            }
            Err(error) => {
                self.entries.clear();
                self.error = Some(format!("{}: {error:#}", self.dir.display()));
            }
        }
        self.selected = self.selected.min(self.entries.len().saturating_sub(1));
    }

    pub fn selection(&self) -> Option<&Entry> {
        self.entries.get(self.selected)
    }

    pub fn move_by(&mut self, delta: i32) {
        if self.entries.is_empty() {
            return;
        }
        let last = self.entries.len() - 1;
        let target = self.selected as i32 + delta;
        self.selected = target.clamp(0, last as i32) as usize;
    }

    pub fn move_to(&mut self, index: usize) {
        self.selected = index.min(self.entries.len().saturating_sub(1));
    }

    /// Descend into a folder or a disc image, keeping the cursor on whatever was just left.
    pub fn enter(&mut self, dir: PathBuf) {
        let leaving = std::mem::replace(&mut self.dir, dir);
        self.selected = 0;
        self.refresh();
        if let Some(index) = self.entries.iter().position(|entry| entry.path == leaving) {
            self.selected = index;
        }
    }

    /// Every recording listed here, in the order the pane shows them.
    pub fn playable(&self) -> Vec<TrackRef> {
        let mut tracks = Vec::new();
        for entry in &self.entries {
            if let Some(track) = entry.kind.track() {
                tracks.push(track.clone());
            }
        }
        tracks
    }
}

fn list(path: &Path) -> Result<Vec<Entry>> {
    if sacd::is_image(path) {
        return list_disc(path);
    }
    list_dir(path)
}

/// The tracks of a disc image, in the order the disc numbers them.
fn list_disc(path: &Path) -> Result<Vec<Entry>> {
    let disc = Disc::open(path)?;
    let mut entries = vec![parent_entry(path)];
    for track in disc.tracks() {
        let tags = disc.tags(track.number);
        entries.push(Entry {
            name: format!("track {}", track.number),
            title: tags.label(),
            path: path.to_path_buf(),
            kind: Kind::Track(TrackRef::of_disc(path.to_path_buf(), track.number)),
        });
    }
    Ok(entries)
}

fn list_dir(dir: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let Some(kind) = kind_of(&path) else {
            continue;
        };
        // A recording that will not open, or carries no tags, still lists under its own name.
        let title = kind
            .track()
            .and_then(|track| reader::tags_of(track).ok())
            .and_then(|tags| tags.label());
        entries.push(Entry {
            name,
            title,
            path,
            kind,
        });
    }
    entries.sort_by(|a, b| {
        b.kind
            .opens()
            .cmp(&a.kind.opens())
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if dir.parent().is_some() {
        entries.insert(0, parent_entry(dir));
    }
    Ok(entries)
}

fn parent_entry(path: &Path) -> Entry {
    Entry {
        name: "..".to_owned(),
        title: None,
        path: path.parent().unwrap_or(path).to_path_buf(),
        kind: Kind::Folder,
    }
}

/// What a path is worth listing as, or nothing when this player cannot open it.
fn kind_of(path: &Path) -> Option<Kind> {
    if path.is_dir() {
        return Some(Kind::Folder);
    }
    if sacd::is_image(path) {
        return Some(Kind::Disc);
    }
    let extension = path.extension()?.to_string_lossy().to_lowercase();
    let playable = extension == "dsf" || extension == "dff" || extension == "flac";
    playable.then(|| Kind::Track(TrackRef::file(path.to_path_buf())))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::reader::dsf::tests::dsf_file_with_tag;
    use crate::tui::browser::{Browser, Kind, list_dir};

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("album")).expect("subdir");
        for name in ["b.dsf", "A.DFF", "c.flac", "notes.txt", ".hidden.dsf"] {
            fs::write(dir.path().join(name), b"").expect("file");
        }
        dir
    }

    #[test]
    fn only_folders_and_playable_files_are_listed_folders_first() {
        let dir = fixture();

        let names: Vec<String> = list_dir(dir.path())
            .expect("reads")
            .into_iter()
            .map(|entry| entry.name)
            .collect();

        assert_eq!(names, ["..", "album", "A.DFF", "b.dsf", "c.flac"]);
    }

    #[test]
    fn a_disc_image_lists_as_something_to_open_rather_than_to_play() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("disc.iso"), b"not really a disc").expect("image");

        let entries = list_dir(dir.path()).expect("reads");

        assert_eq!(entries[1].kind, Kind::Disc);
        assert!(entries[1].kind.opens());
    }

    #[test]
    fn the_playlist_is_the_recordings_of_the_current_folder_in_pane_order() {
        let dir = fixture();
        let browser = Browser::open(dir.path().to_path_buf());

        let files: Vec<_> = browser
            .playable()
            .into_iter()
            .map(|track| track.path)
            .collect();

        assert_eq!(
            files,
            [
                dir.path().join("A.DFF"),
                dir.path().join("b.dsf"),
                dir.path().join("c.flac")
            ]
        );
    }

    #[test]
    fn descending_and_going_back_up_leaves_the_cursor_on_the_folder_just_left() {
        let dir = fixture();
        let mut browser = Browser::open(dir.path().to_path_buf());
        browser.move_to(1);
        let album = browser.selection().expect("album row").path.clone();

        browser.enter(album.clone());
        assert_eq!(browser.dir, album);
        browser.enter(dir.path().to_path_buf());

        assert_eq!(browser.selection().expect("album row").path, album);
    }

    #[test]
    fn a_tagged_file_lists_under_its_title_and_an_untagged_one_under_its_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("01.dsf"), dsf_file_with_tag("So What")).expect("tagged");
        fs::write(dir.path().join("02.dsf"), b"not a dsd file at all").expect("untagged");

        let entries = list_dir(dir.path()).expect("reads");

        let titles: Vec<Option<&str>> =
            entries.iter().map(|entry| entry.title.as_deref()).collect();
        assert_eq!(titles, [None, Some("So What"), None]);
    }

    #[test]
    fn the_cursor_stops_at_the_ends_of_the_list() {
        let dir = fixture();
        let mut browser = Browser::open(dir.path().to_path_buf());

        browser.move_by(-5);
        assert_eq!(browser.selected, 0);
        browser.move_by(99);
        assert_eq!(browser.selected, browser.entries.len() - 1);
    }
}
