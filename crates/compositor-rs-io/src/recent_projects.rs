//! File > Open Recent. macOS keeps the list (the same one the Dock icon's menu shows); this mirrors
//! it so the menu updates as projects are opened and saved. Projects since moved or deleted are left
//! out, checked again each time you come back to the app (from Finder, say).

use compositor_rs_core::settings;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::LazyLock;

/// The most recent projects macOS keeps (`NSDocumentController.maximumRecentDocumentCount`).
pub const MAXIMUM_RECENT_PROJECTS: usize = 10;

/// The file the list persists in, inside the config directory the tool defaults use.
const FILE_NAME: &str = "recent-projects.json";

/// What the file holds: the project paths, most recent first.
#[derive(Serialize, Deserialize)]
struct StoredRecentProjects {
    urls: Vec<String>,
}

/// The recent-project list, in the order the File > Open Recent menu shows it.
pub struct RecentProjects {
    urls: Vec<PathBuf>,
    path: PathBuf,
}

impl RecentProjects {
    /// The one list the app uses. macOS keeps its own across launches; this reads it back at startup.
    pub fn shared() -> &'static Mutex<RecentProjects> {
        static SHARED: LazyLock<Mutex<RecentProjects>> = LazyLock::new(|| Mutex::new(RecentProjects::load()));
        &SHARED
    }

    /// Reads the list where the app keeps it.
    pub fn load() -> Self {
        Self::load_from(settings::config_directory().join(FILE_NAME))
    }

    /// Reads the list from `path`; a missing or damaged file starts the list empty.
    pub fn load_from(path: PathBuf) -> Self {
        let mut list = RecentProjects { urls: Vec::new(), path };
        if let Ok(text) = std::fs::read_to_string(&list.path) {
            if let Ok(stored) = serde_json::from_str::<StoredRecentProjects>(&text) {
                list.urls = stored.urls.into_iter().map(PathBuf::from).collect();
                list.urls.truncate(MAXIMUM_RECENT_PROJECTS);
            }
        }
        list.refresh();
        list
    }

    /// The project paths, most recent first, with the missing ones left out.
    pub fn urls(&self) -> &[PathBuf] {
        &self.urls
    }

    /// A project was opened or saved: it goes to the front of the list.
    pub fn note(&mut self, url: PathBuf) {
        self.urls.retain(|existing| existing != &url);
        self.urls.insert(0, url);
        self.urls.truncate(MAXIMUM_RECENT_PROJECTS);
        self.urls.retain(|url| url.exists());
        self.persist();
    }

    /// File > Open Recent > Clear Menu.
    pub fn clear(&mut self) {
        self.urls.clear();
        self.persist();
    }

    /// Drops the projects since moved or deleted; macOS does this every time the app is activated.
    pub fn refresh(&mut self) {
        self.urls.retain(|url| url.exists());
        self.persist();
    }

    fn persist(&self) {
        let stored = StoredRecentProjects {
            urls: self.urls.iter().map(|url| url.to_string_lossy().into_owned()).collect(),
        };
        if let Some(directory) = self.path.parent() {
            let _ = std::fs::create_dir_all(directory);
        }
        if let Ok(text) = serde_json::to_string_pretty(&stored) {
            let _ = std::fs::write(&self.path, text);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(name: &str) -> RecentProjects {
        let path = std::env::temp_dir().join(format!("compositor-recent-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        RecentProjects { urls: Vec::new(), path }
    }

    /// A path that exists, so `refresh` keeps it.
    fn existing(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("compositor-recent-{name}-{}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn the_newest_project_comes_first_and_never_repeats() {
        let mut recents = list("order");
        let a = existing("a");
        let b = existing("b");
        recents.note(a.clone());
        recents.note(b.clone());
        assert_eq!(recents.urls(), &[b.clone(), a.clone()]);
        // Noting an older project again moves it to the front instead of adding it twice.
        recents.note(a.clone());
        assert_eq!(recents.urls(), &[a.clone(), b.clone()]);
        std::fs::remove_file(a).unwrap();
        std::fs::remove_file(b).unwrap();
    }

    #[test]
    fn the_list_keeps_the_ten_most_recent_projects() {
        let mut recents = list("cap");
        let mut paths = Vec::new();
        for index in 0..12 {
            let path = existing(&format!("cap-{index}"));
            recents.note(path.clone());
            paths.push(path);
        }
        assert_eq!(recents.urls().len(), MAXIMUM_RECENT_PROJECTS);
        // The last ten noted, newest first.
        let expected: Vec<PathBuf> = paths.iter().rev().take(MAXIMUM_RECENT_PROJECTS).cloned().collect();
        assert_eq!(recents.urls(), expected.as_slice());
        for path in paths {
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn refresh_drops_projects_that_are_gone_and_clear_empties_it() {
        let mut recents = list("refresh");
        let kept = existing("kept");
        let gone = existing("gone");
        recents.note(gone.clone());
        recents.note(kept.clone());
        std::fs::remove_file(&gone).unwrap();
        recents.refresh();
        assert_eq!(recents.urls(), &[kept.clone()]);

        recents.clear();
        assert!(recents.urls().is_empty());
        std::fs::remove_file(kept).unwrap();
    }

    #[test]
    fn the_list_round_trips_through_the_file_in_order() {
        let path = std::env::temp_dir().join(format!("compositor-recent-store-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let first = existing("first");
        let second = existing("second");
        {
            let mut recents = RecentProjects::load_from(path.clone());
            recents.note(first.clone());
            recents.note(second.clone());
        }
        let reloaded = RecentProjects::load_from(path.clone());
        assert_eq!(reloaded.urls(), &[second.clone(), first.clone()]);

        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(first).unwrap();
        std::fs::remove_file(second).unwrap();
    }
}
