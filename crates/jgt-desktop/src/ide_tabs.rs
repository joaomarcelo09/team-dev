//! Pure tab state for the IDE viewer (the files the user opened, like VS Code tabs).

use std::path::{Path, PathBuf};

/// Maximum number of open tabs; opening more closes the oldest non-active one.
pub const MAX_TABS: usize = 20;

/// One open file: which source it belongs to, its path, and its File/Diff state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdeTab {
    pub source: crate::ide::Source,
    pub path: String,
    pub show_diff: bool,
}

impl IdeTab {
    pub fn new(source: crate::ide::Source, path: impl Into<String>, show_diff: bool) -> Self {
        Self { source, path: path.into(), show_diff }
    }

    /// The file name shown on the tab (last path component).
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    fn same_file(&self, other: &IdeTab) -> bool {
        self.source.root == other.source.root && self.path == other.path
    }
}

/// The ordered list of open tabs and the active one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdeTabs {
    pub tabs: Vec<IdeTab>,
    pub active: Option<usize>,
}

impl IdeTabs {
    pub fn active_tab(&self) -> Option<&IdeTab> {
        self.active.and_then(|i| self.tabs.get(i))
    }

    pub fn find(&self, root: &Path, path: &str) -> Option<usize> {
        self.tabs.iter().position(|t| t.source.root == root && t.path == path)
    }

    /// Focuses the tab with the same root and path, or appends and activates a new one.
    /// Past [`MAX_TABS`] the oldest non-active tab is removed. Returns the active index.
    pub fn open(&mut self, tab: IdeTab) -> usize {
        if let Some(i) = self.tabs.iter().position(|t| t.same_file(&tab)) {
            self.active = Some(i);
            return i;
        }
        self.tabs.push(tab);
        let new_index = self.tabs.len() - 1;
        self.active = Some(new_index);
        while self.tabs.len() > MAX_TABS {
            // The new tab is the active one, so the oldest non-active one is index 0.
            let Some(oldest) = (0..self.tabs.len()).find(|&i| Some(i) != self.active) else {
                break;
            };
            self.remove_at(oldest);
        }
        self.active.unwrap_or(new_index)
    }

    /// Activates a tab. Returns false when the index is out of range.
    pub fn focus(&mut self, index: usize) -> bool {
        if index < self.tabs.len() {
            self.active = Some(index);
            true
        } else {
            false
        }
    }

    /// Closes a tab. Closing the active one activates its right neighbour (or the left one
    /// when it was the last); closing the only tab leaves none active.
    pub fn close(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        let was_active = self.active == Some(index);
        self.remove_at(index);
        if was_active {
            self.active = if self.tabs.is_empty() { None } else { Some(index.min(self.tabs.len() - 1)) };
        }
    }

    /// Sets the File/Diff state of the active tab.
    pub fn set_diff(&mut self, show_diff: bool) {
        if let Some(tab) = self.active.and_then(|i| self.tabs.get_mut(i)) {
            tab.show_diff = show_diff;
        }
    }

    /// Drops tabs whose source root is not in `roots`. The active tab keeps its focus when it
    /// survives; otherwise no tab is active.
    pub fn retain_roots(&mut self, roots: &[PathBuf]) {
        let mut i = 0;
        while i < self.tabs.len() {
            if roots.iter().any(|r| r == &self.tabs[i].source.root) {
                i += 1;
            } else {
                let was_active = self.active == Some(i);
                self.remove_at(i);
                if was_active {
                    self.active = None;
                }
            }
        }
    }

    /// Removes a tab and shifts the active index so it keeps pointing at the same tab.
    fn remove_at(&mut self, index: usize) {
        self.tabs.remove(index);
        self.active = match self.active {
            Some(a) if a == index => None,
            Some(a) if a > index => Some(a - 1),
            other => other,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ide::{Source, SourceKind};

    fn main_src() -> Source {
        Source { label: "Main".to_string(), root: PathBuf::from("/p"), kind: SourceKind::Main }
    }

    fn wt_src(name: &str) -> Source {
        Source {
            label: name.to_string(),
            root: PathBuf::from(format!("/p/.team/worktrees/{name}")),
            kind: SourceKind::Worktree { branch: format!("jgt/{name}") },
        }
    }

    fn tab(src: &Source, path: &str) -> IdeTab {
        IdeTab::new(src.clone(), path, false)
    }

    #[test]
    fn open_appends_and_activates() {
        let mut t = IdeTabs::default();
        assert_eq!(t.open(tab(&main_src(), "a.rs")), 0);
        assert_eq!(t.open(tab(&main_src(), "b.rs")), 1);
        assert_eq!(t.tabs.len(), 2);
        assert_eq!(t.active, Some(1));
        assert_eq!(t.active_tab().unwrap().path, "b.rs");
    }

    #[test]
    fn open_existing_focuses_it() {
        let mut t = IdeTabs::default();
        t.open(tab(&main_src(), "a.rs"));
        t.open(tab(&main_src(), "b.rs"));
        assert_eq!(t.open(tab(&main_src(), "a.rs")), 0);
        assert_eq!(t.tabs.len(), 2);
        assert_eq!(t.active, Some(0));
        // Same path in another source is a different tab.
        assert_eq!(t.open(tab(&wt_src("jgt-1"), "a.rs")), 2);
        assert_eq!(t.tabs.len(), 3);
    }

    #[test]
    fn focus_checks_range() {
        let mut t = IdeTabs::default();
        t.open(tab(&main_src(), "a.rs"));
        t.open(tab(&main_src(), "b.rs"));
        assert!(t.focus(0));
        assert_eq!(t.active, Some(0));
        assert!(!t.focus(5));
        assert_eq!(t.active, Some(0));
    }

    #[test]
    fn close_active_activates_neighbour() {
        let mut t = IdeTabs::default();
        for p in ["a", "b", "c"] {
            t.open(tab(&main_src(), p));
        }
        t.focus(1);
        t.close(1);
        assert_eq!(t.active_tab().unwrap().path, "c");
        // Closing the last (active) tab activates the left one.
        t.close(1);
        assert_eq!(t.active_tab().unwrap().path, "a");
        t.close(0);
        assert!(t.tabs.is_empty());
        assert_eq!(t.active, None);
    }

    #[test]
    fn close_inactive_keeps_active_tab() {
        let mut t = IdeTabs::default();
        for p in ["a", "b", "c"] {
            t.open(tab(&main_src(), p));
        }
        t.focus(2);
        t.close(0);
        assert_eq!(t.active, Some(1));
        assert_eq!(t.active_tab().unwrap().path, "c");
        t.close(1 + 5); // out of range: no-op
        assert_eq!(t.tabs.len(), 2);
    }

    #[test]
    fn limit_removes_oldest_non_active() {
        let mut t = IdeTabs::default();
        for i in 0..MAX_TABS {
            t.open(tab(&main_src(), &format!("f{i}")));
        }
        assert_eq!(t.tabs.len(), MAX_TABS);
        let idx = t.open(tab(&main_src(), "new"));
        assert_eq!(t.tabs.len(), MAX_TABS);
        assert_eq!(t.tabs[idx].path, "new");
        assert_eq!(t.active, Some(idx));
        assert!(t.find(Path::new("/p"), "f0").is_none());
        assert!(t.find(Path::new("/p"), "f1").is_some());
    }

    #[test]
    fn limit_keeps_the_active_tab() {
        let mut t = IdeTabs::default();
        for i in 0..MAX_TABS {
            t.open(tab(&main_src(), &format!("f{i}")));
        }
        // Re-focusing f0 then opening a new one: the new tab is active, f0 is the oldest
        // non-active and goes; the active one is never removed.
        t.open(tab(&main_src(), "f0"));
        t.open(tab(&main_src(), "new"));
        assert_eq!(t.tabs.len(), MAX_TABS);
        assert_eq!(t.active_tab().unwrap().path, "new");
        assert!(t.find(Path::new("/p"), "f0").is_none());
    }

    #[test]
    fn set_diff_updates_active_tab() {
        let mut t = IdeTabs::default();
        t.open(tab(&main_src(), "a"));
        t.open(tab(&main_src(), "b"));
        t.set_diff(true);
        assert!(t.tabs[1].show_diff);
        assert!(!t.tabs[0].show_diff);
    }

    #[test]
    fn retain_roots_drops_missing_sources() {
        let mut t = IdeTabs::default();
        let gone = wt_src("jgt-1");
        let kept = wt_src("jgt-2");
        t.open(tab(&main_src(), "a"));
        t.open(tab(&gone, "b"));
        t.open(tab(&kept, "c"));
        t.retain_roots(&[PathBuf::from("/p"), kept.root.clone()]);
        assert_eq!(t.tabs.len(), 2);
        assert_eq!(t.active_tab().unwrap().path, "c");

        t.focus(1);
        t.retain_roots(&[PathBuf::from("/p")]);
        assert_eq!(t.tabs.len(), 1);
        assert_eq!(t.active, None);
    }

    #[test]
    fn tab_name_is_last_component() {
        assert_eq!(tab(&main_src(), "src/app/main.rs").name(), "main.rs");
        assert_eq!(tab(&main_src(), "README.md").name(), "README.md");
    }
}
