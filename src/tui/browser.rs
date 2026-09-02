use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::reader;

/// One row of the file pane: a folder to descend into, or a file this player can open.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    /// What the file's tags call it, when it carries any.
    pub title: Option<String>,
    pub path: PathBuf,
    pub is_dir: bool,
}

/// A cursor over one directory, showing only folders and DSD files.
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
        match read_dir(&self.dir) {
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

    /// Descend into a folder, keeping the cursor on the folder just left when going up.
    pub fn enter(&mut self, dir: PathBuf) {
        let leaving = std::mem::replace(&mut self.dir, dir);
        self.selected = 0;
        self.refresh();
        if let Some(index) = self.entries.iter().position(|entry| entry.path == leaving) {
            self.selected = index;
        }
    }

    /// Every playable file in the current directory, in the order the pane lists them.
    pub fn playable(&self) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for entry in &self.entries {
            if !entry.is_dir {
                files.push(entry.path.clone());
            }
        }
        files
    }
}

fn read_dir(dir: &Path) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        let is_dir = path.is_dir();
        if !is_dir && !is_dsd_file(&path) {
            continue;
        }
        // A file that will not open, or carries no tags, still lists under its own name.
        let title = if is_dir {
            None
        } else {
            reader::tags_of(&path).ok().and_then(|tags| tags.label())
        };
        entries.push(Entry {
            name,
            title,
            path,
            is_dir,
        });
    }
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    if let Some(parent) = dir.parent() {
        entries.insert(
            0,
            Entry {
                name: "..".to_owned(),
                title: None,
                path: parent.to_path_buf(),
                is_dir: true,
            },
        );
    }
    Ok(entries)
}

fn is_dsd_file(path: &Path) -> bool {
    let Some(extension) = path.extension() else {
        return false;
    };
    let extension = extension.to_string_lossy().to_lowercase();
    extension == "dsf" || extension == "dff"
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::reader::dsf::tests::dsf_file_with_tag;
    use crate::tui::browser::{Browser, read_dir};

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir(dir.path().join("album")).expect("subdir");
        for name in ["b.dsf", "A.DFF", "notes.txt", ".hidden.dsf"] {
            fs::write(dir.path().join(name), b"").expect("file");
        }
        dir
    }

    #[test]
    fn only_folders_and_dsd_files_are_listed_folders_first() {
        let dir = fixture();

        let names: Vec<String> = read_dir(dir.path())
            .expect("reads")
            .into_iter()
            .map(|entry| entry.name)
            .collect();

        assert_eq!(names, ["..", "album", "A.DFF", "b.dsf"]);
    }

    #[test]
    fn the_playlist_is_the_files_of_the_current_directory_in_pane_order() {
        let dir = fixture();
        let browser = Browser::open(dir.path().to_path_buf());

        let files = browser.playable();

        assert_eq!(files, [dir.path().join("A.DFF"), dir.path().join("b.dsf")]);
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

        let entries = read_dir(dir.path()).expect("reads");

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
