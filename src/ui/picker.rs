//! A file browser for choosing attachments without leaving the chat.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

pub struct Entry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
}

pub struct Picker {
    pub dir: PathBuf,
    pub filter: String,
    pub selected: usize,
    pub marked: BTreeSet<PathBuf>,
    pub error: Option<String>,
    all: Vec<Entry>,
}

pub enum Outcome {
    Stay,
    Chosen(Vec<PathBuf>),
}

impl Picker {
    pub fn new(dir: PathBuf) -> Self {
        let mut picker = Self {
            dir,
            filter: String::new(),
            selected: 0,
            marked: BTreeSet::new(),
            error: None,
            all: Vec::new(),
        };
        picker.reload();
        picker
    }

    fn reload(&mut self) {
        self.all.clear();
        self.selected = 0;
        self.error = None;
        match std::fs::read_dir(&self.dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    // Follows symlinks, so a link to a folder opens like one.
                    let meta = std::fs::metadata(entry.path());
                    let dir = meta.as_ref().is_ok_and(|m| m.is_dir());
                    let size = meta.map(|m| m.len()).unwrap_or(0);
                    self.all.push(Entry { name, dir, size });
                }
            }
            Err(e) => self.error = Some(e.to_string()),
        }
        self.all.sort_by(|a, b| {
            b.dir
                .cmp(&a.dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
    }

    /// Entries matching the filter. Dotfiles show only when the filter
    /// starts with a dot.
    pub fn visible(&self) -> Vec<&Entry> {
        let filter = self.filter.to_lowercase();
        let hidden = filter.starts_with('.');
        self.all
            .iter()
            .filter(|e| hidden || !e.name.starts_with('.'))
            .filter(|e| filter.is_empty() || fuzzy(&e.name.to_lowercase(), &filter))
            .collect()
    }

    pub fn path_of(&self, entry: &Entry) -> PathBuf {
        self.dir.join(&entry.name)
    }

    fn current(&self) -> Option<&Entry> {
        self.visible().get(self.selected).copied()
    }

    pub fn up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn down(&mut self) {
        let n = self.visible().len();
        if self.selected + 1 < n {
            self.selected += 1;
        }
    }

    pub fn page(&mut self, rows: isize) {
        let n = self.visible().len() as isize;
        self.selected = (self.selected as isize + rows).clamp(0, (n - 1).max(0)) as usize;
    }

    pub fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.selected = 0;
    }

    /// Backspace: trims the filter, or goes up a folder once it is empty.
    pub fn backspace(&mut self) {
        if self.filter.pop().is_none() {
            self.parent();
        }
        self.selected = 0;
    }

    pub fn parent(&mut self) {
        let here = self.dir.clone();
        if let Some(parent) = here.parent() {
            self.dir = parent.to_path_buf();
            self.filter.clear();
            self.reload();
            // Land on the folder we came from.
            let name = here.file_name().map(|n| n.to_string_lossy().into_owned());
            if let Some(i) = self.visible().iter().position(|e| Some(&e.name) == name.as_ref()) {
                self.selected = i;
            }
        }
    }

    pub fn home(&mut self) {
        if let Some(home) = home_dir() {
            self.enter(home);
        }
    }

    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.filter.clear();
        self.reload();
    }

    /// Space: marks or unmarks the file under the cursor.
    pub fn toggle_mark(&mut self) {
        if let Some(entry) = self.current().filter(|e| !e.dir) {
            let path = self.path_of(entry);
            if !self.marked.remove(&path) {
                self.marked.insert(path);
            }
            self.down();
        }
    }

    /// Enter: opens a folder, or picks the marked files (or the current one).
    pub fn choose(&mut self) -> Outcome {
        let Some(entry) = self.current() else {
            return Outcome::Stay;
        };
        if entry.dir {
            let dir = self.path_of(entry);
            self.enter(dir);
            return Outcome::Stay;
        }
        let path = self.path_of(entry);
        if self.marked.is_empty() {
            Outcome::Chosen(vec![path])
        } else {
            Outcome::Chosen(std::mem::take(&mut self.marked).into_iter().collect())
        }
    }

    pub fn is_marked(&self, entry: &Entry) -> bool {
        self.marked.contains(&self.path_of(entry))
    }
}

/// Every filter char appears in `name`, in order.
fn fuzzy(name: &str, filter: &str) -> bool {
    let mut chars = name.chars();
    filter.chars().all(|f| chars.any(|c| c == f))
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// `~/x` for paths under home, for display.
pub fn tilde(path: &Path) -> String {
    if let Some(home) = home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        return if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.display())
        };
    }
    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    #[test]
    fn fuzzy_matches_in_order() {
        assert!(fuzzy("report-final.pdf", "rpf"));
        assert!(!fuzzy("report.pdf", "fdr"));
    }
}
