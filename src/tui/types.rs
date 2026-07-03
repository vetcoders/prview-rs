//! Type definitions for the TUI module.
//!
//! Contains all state structures, enums, and navigation helpers
//! inspired by rmcp_mux wizard patterns.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::Config;
use crate::checks::CheckResult;
use crate::git::Diff;
use crate::heuristics::HeuristicsResult;
use crate::output::Report;

// ─────────────────────────────────────────────────────────────────────────────
// Navigation Enums
// ─────────────────────────────────────────────────────────────────────────────

/// Main panel selection
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Panel {
    #[default]
    Config,
    Checks,
    Diffs,
    Artifacts,
    Heuristics,
    State,
}

/// Wizard mode for initial setup
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WizardMode {
    /// No wizard - normal TUI mode
    #[default]
    None,
    /// Selecting target branch
    SelectTarget,
    /// Selecting base branches
    SelectBase,
}

/// Branch selection state for wizard
#[derive(Debug, Clone, Default)]
pub struct BranchSelectorState {
    /// All available local branches
    pub local_branches: Vec<String>,
    /// All available remote branches
    pub remote_branches: Vec<String>,
    /// Currently selected index in combined list
    pub selected: usize,
    /// Scroll offset for display
    pub scroll_offset: usize,
    /// Current branch (marked with *)
    pub current_branch: Option<String>,
    /// Whether showing local (true) or remote (false) branches
    pub show_local: bool,
    /// Selected bases (for multi-select in base mode)
    pub selected_bases: Vec<String>,
    /// Filter string for searching
    pub filter: String,
}

const PANEL_COUNT: usize = 6;

impl Panel {
    pub fn index(&self) -> usize {
        match self {
            Panel::Config => 0,
            Panel::Checks => 1,
            Panel::Diffs => 2,
            Panel::Artifacts => 3,
            Panel::Heuristics => 4,
            Panel::State => 5,
        }
    }

    pub fn from_index(idx: usize) -> Self {
        match idx % PANEL_COUNT {
            0 => Panel::Config,
            1 => Panel::Checks,
            2 => Panel::Diffs,
            3 => Panel::Artifacts,
            4 => Panel::Heuristics,
            5 => Panel::State,
            _ => Panel::Config,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Panel::Config => "Config",
            Panel::Checks => "Checks",
            Panel::Diffs => "Diffs",
            Panel::Artifacts => "Artifacts",
            Panel::Heuristics => "Heuristics",
            Panel::State => "State",
        }
    }

    pub fn next(&self) -> Self {
        Self::from_index((self.index() + 1) % PANEL_COUNT)
    }

    pub fn prev(&self) -> Self {
        Self::from_index((self.index() + PANEL_COUNT - 1) % PANEL_COUNT)
    }
}

/// Lifecycle state of a check in the TUI (Pending → Running → Completed/Failed/Skipped/Cached).
/// Not to be confused with `crate::checks::CheckStatus`, which is the quality outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CheckLifecycle {
    #[default]
    Pending,
    Running,
    Completed,
    /// Check finished with non-blocking warnings (quality outcome `Warnings`).
    /// Kept distinct from `Completed` so the TUI never renders a warned check
    /// as a clean pass.
    Warned,
    Failed,
    Skipped,
    Cached,
}

impl CheckLifecycle {
    pub fn icon(&self) -> &'static str {
        match self {
            CheckLifecycle::Pending => "○",
            CheckLifecycle::Running => "◐",
            CheckLifecycle::Completed => "●",
            CheckLifecycle::Warned => "⚠",
            CheckLifecycle::Failed => "✗",
            CheckLifecycle::Skipped => "○",
            CheckLifecycle::Cached => "●",
        }
    }
}

/// File change type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl FileChangeKind {
    pub fn icon(&self) -> &'static str {
        match self {
            FileChangeKind::Added => "A",
            FileChangeKind::Modified => "M",
            FileChangeKind::Deleted => "D",
            FileChangeKind::Renamed => "R",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// State Structures
// ─────────────────────────────────────────────────────────────────────────────

/// Entry for a check in the checks panel
#[derive(Debug, Clone)]
pub struct CheckEntry {
    pub name: String,
    pub status: CheckLifecycle,
    pub duration: Option<Duration>,
    pub output: String,
    pub cached: bool,
}

impl CheckEntry {
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_string(),
            status: CheckLifecycle::Pending,
            duration: None,
            output: String::new(),
            cached: false,
        }
    }

    pub fn from_result(result: &CheckResult) -> Self {
        use crate::checks::CheckStatus as CrateCheckStatus;

        let status = match result.status {
            CrateCheckStatus::Passed => {
                if result.cached {
                    CheckLifecycle::Cached
                } else {
                    CheckLifecycle::Completed
                }
            }
            CrateCheckStatus::Failed => CheckLifecycle::Failed,
            CrateCheckStatus::Warnings => CheckLifecycle::Warned,
            CrateCheckStatus::Skipped => CheckLifecycle::Skipped,
            CrateCheckStatus::Error => CheckLifecycle::Failed,
        };

        Self {
            name: result.name.clone(),
            status,
            duration: Some(result.duration),
            output: result.output.clone(),
            cached: result.cached,
        }
    }
}

/// Entry for a file in the diffs panel
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub path: String,
    pub kind: FileChangeKind,
    pub additions: usize,
    pub deletions: usize,
    pub base_ref: Option<String>,
    pub target_ref: Option<String>,
}

/// Entry for an artifact in the artifacts panel
#[derive(Debug, Clone)]
pub struct ArtifactEntry {
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
    pub is_dir: bool,
}

/// Editable fields in config panel
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConfigField {
    #[default]
    Branches,
    RunTests,
    RunLint,
    RunBundle,
    RunSecurity,
    RunHeuristics,
    UseCache,
}

impl ConfigField {
    pub fn next(&self) -> Self {
        match self {
            ConfigField::Branches => ConfigField::RunTests,
            ConfigField::RunTests => ConfigField::RunLint,
            ConfigField::RunLint => ConfigField::RunBundle,
            ConfigField::RunBundle => ConfigField::RunSecurity,
            ConfigField::RunSecurity => ConfigField::RunHeuristics,
            ConfigField::RunHeuristics => ConfigField::UseCache,
            ConfigField::UseCache => ConfigField::Branches,
        }
    }

    pub fn prev(&self) -> Self {
        match self {
            ConfigField::Branches => ConfigField::UseCache,
            ConfigField::RunTests => ConfigField::Branches,
            ConfigField::RunLint => ConfigField::RunTests,
            ConfigField::RunBundle => ConfigField::RunLint,
            ConfigField::RunSecurity => ConfigField::RunBundle,
            ConfigField::RunHeuristics => ConfigField::RunSecurity,
            ConfigField::UseCache => ConfigField::RunHeuristics,
        }
    }
}

/// Config panel state
#[derive(Debug, Clone, Default)]
pub struct ConfigPanelState {
    pub scroll_offset: usize,
    pub selected_field: ConfigField,
}

/// Checks panel state
#[derive(Debug, Clone, Default)]
pub struct ChecksPanelState {
    pub entries: Vec<CheckEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
}

/// Diffs panel state
#[derive(Debug, Clone, Default)]
pub struct DiffsPanelState {
    pub files: Vec<FileEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
    pub show_diff: bool,
    pub diff_content: String,
    pub diff_scroll: u16,
    pub commits_count: usize,
    pub total_additions: usize,
    pub total_deletions: usize,
}

/// Artifacts panel state
#[derive(Debug, Clone, Default)]
pub struct ArtifactsPanelState {
    pub entries: Vec<ArtifactEntry>,
    pub selected: usize,
    pub scroll_offset: usize,
}

/// Heuristics panel state
#[derive(Debug, Clone, Default)]
pub struct HeuristicsPanelState {
    pub dead_exports_count: usize,
    pub cycles_count: usize,
    pub twins_count: usize,
    pub selected_section: usize,
    pub scroll_offset: usize,
}

/// State panel state (repo probe viewer)
#[derive(Debug, Clone, Default)]
pub struct StatePanelState {
    pub repo_state: Option<crate::state::RepoState>,
    pub scroll_offset: usize,
}

// ─────────────────────────────────────────────────────────────────────────────
// Main Application State
// ─────────────────────────────────────────────────────────────────────────────

/// Main TUI application state
pub struct TuiState {
    // Navigation
    pub current_panel: Panel,
    pub show_help: bool,

    // Config reference
    pub config: Config,

    // Git info
    pub target_branch: String,
    pub base_branches: Vec<String>,

    // Wizard mode for branch selection
    pub wizard_mode: WizardMode,
    pub branch_selector: BranchSelectorState,

    // Panel states
    pub config_state: ConfigPanelState,
    pub checks_state: ChecksPanelState,
    pub diffs_state: DiffsPanelState,
    pub artifacts_state: ArtifactsPanelState,
    pub heuristics_state: HeuristicsPanelState,
    pub state_panel: StatePanelState,

    // Global state
    pub message: String,
    pub running: bool,
    pub start_time: Option<Instant>,
    pub report: Option<Report>,

    // Quit flag
    pub should_quit: bool,
}

impl TuiState {
    pub fn new(config: Config) -> Self {
        Self {
            current_panel: Panel::Config,
            show_help: false,
            config,
            target_branch: String::new(),
            base_branches: Vec::new(),
            wizard_mode: WizardMode::None,
            branch_selector: BranchSelectorState::default(),
            config_state: ConfigPanelState::default(),
            checks_state: ChecksPanelState::default(),
            diffs_state: DiffsPanelState::default(),
            artifacts_state: ArtifactsPanelState::default(),
            heuristics_state: HeuristicsPanelState::default(),
            state_panel: StatePanelState::default(),
            message: "Press [r] to run analysis, [?] for help".to_string(),
            running: false,
            start_time: None,
            report: None,
            should_quit: false,
        }
    }

    /// Create a state-only TUI view (for `prview state --tui`)
    pub fn new_state_view(config: Config, repo_state: crate::state::RepoState) -> Self {
        Self {
            current_panel: Panel::State,
            show_help: false,
            config,
            target_branch: String::new(),
            base_branches: Vec::new(),
            wizard_mode: WizardMode::None,
            branch_selector: BranchSelectorState::default(),
            config_state: ConfigPanelState::default(),
            checks_state: ChecksPanelState::default(),
            diffs_state: DiffsPanelState::default(),
            artifacts_state: ArtifactsPanelState::default(),
            heuristics_state: HeuristicsPanelState::default(),
            state_panel: StatePanelState {
                repo_state: Some(repo_state),
                scroll_offset: 0,
            },
            message: "[↑↓]scroll [1-6]panel [?]help [q]uit".to_string(),
            running: false,
            start_time: None,
            report: None,
            should_quit: false,
        }
    }

    /// Start branch selection wizard for target branch
    pub fn start_target_wizard(&mut self) {
        self.wizard_mode = WizardMode::SelectTarget;
        self.branch_selector.selected = 0;
        self.branch_selector.scroll_offset = 0;
        self.branch_selector.show_local = true;
        self.branch_selector.filter.clear();

        // Pre-select current branch if any
        if let Some(ref current) = self.branch_selector.current_branch
            && let Some(idx) = self
                .branch_selector
                .local_branches
                .iter()
                .position(|b| b == current)
        {
            self.branch_selector.selected = idx;
        }
    }

    /// Start branch selection wizard for base branches
    pub fn start_base_wizard(&mut self) {
        self.wizard_mode = WizardMode::SelectBase;
        self.branch_selector.selected = 0;
        self.branch_selector.scroll_offset = 0;
        self.branch_selector.show_local = true;
        self.branch_selector.selected_bases.clear();
        self.branch_selector.filter.clear();
    }

    /// Get filtered branch list based on current mode
    pub fn get_filtered_branches(&self) -> Vec<&String> {
        let branches = if self.branch_selector.show_local {
            &self.branch_selector.local_branches
        } else {
            &self.branch_selector.remote_branches
        };

        if self.branch_selector.filter.is_empty() {
            branches.iter().collect()
        } else {
            let filter = self.branch_selector.filter.to_lowercase();
            branches
                .iter()
                .filter(|b| b.to_lowercase().contains(&filter))
                .collect()
        }
    }

    /// Select current branch in wizard
    pub fn select_branch(&mut self) {
        // Clone the branch name to avoid borrow issues
        let branch = {
            let filtered = self.get_filtered_branches();
            filtered
                .get(self.branch_selector.selected)
                .map(|b| (*b).clone())
        };

        if let Some(branch) = branch {
            match self.wizard_mode {
                WizardMode::SelectTarget => {
                    self.target_branch = branch.clone();
                    self.config.target = Some(branch);
                    self.config.remote_mode = !self.branch_selector.show_local;
                    // Move to base selection
                    self.start_base_wizard();
                }
                WizardMode::SelectBase => {
                    // Toggle selection for multi-select
                    if self.branch_selector.selected_bases.contains(&branch) {
                        self.branch_selector.selected_bases.retain(|b| b != &branch);
                    } else {
                        self.branch_selector.selected_bases.push(branch);
                    }
                }
                WizardMode::None => {}
            }
        }
    }

    /// Finish base selection wizard
    pub fn finish_base_selection(&mut self) {
        if !self.branch_selector.selected_bases.is_empty() {
            self.base_branches = self.branch_selector.selected_bases.clone();
            self.config.bases = self.branch_selector.selected_bases.clone();
        }
        self.wizard_mode = WizardMode::None;
        self.message = "Branch selection complete! Press [r] to run analysis.".to_string();
    }

    /// Move selection up in branch list
    pub fn branch_select_up(&mut self) {
        let count = self.get_filtered_branches().len();
        if count > 0 && self.branch_selector.selected > 0 {
            self.branch_selector.selected -= 1;
            // Adjust scroll offset
            if self.branch_selector.selected < self.branch_selector.scroll_offset {
                self.branch_selector.scroll_offset = self.branch_selector.selected;
            }
        }
    }

    /// Move selection down in branch list
    pub fn branch_select_down(&mut self) {
        let count = self.get_filtered_branches().len();
        if count > 0 && self.branch_selector.selected < count - 1 {
            self.branch_selector.selected += 1;
        }
    }

    /// Toggle between local and remote branches
    pub fn toggle_branch_source(&mut self) {
        self.branch_selector.show_local = !self.branch_selector.show_local;
        self.branch_selector.selected = 0;
        self.branch_selector.scroll_offset = 0;
    }

    /// Update message based on current panel
    pub fn update_message(&mut self) {
        if self.running {
            let elapsed = self.start_time.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            self.message = format!("Running analysis... {}s", elapsed);
            return;
        }

        self.message = match self.current_panel {
            Panel::Config => "[r]un [1-6]panel [Tab]next [q]uit [?]help".to_string(),
            Panel::Checks => "[r]un [↑↓]select [1-6]panel [Tab]next [q]uit".to_string(),
            Panel::Diffs => {
                "[Enter]file diff [d]full diff [↑↓]select [1-6]panel [q]uit".to_string()
            }
            Panel::Artifacts => "[Enter]open [c]show path [1-6]panel [Tab]next [q]uit".to_string(),
            Panel::Heuristics => "[↑↓]select [1-6]panel [Tab]next [q]uit".to_string(),
            Panel::State => "[↑↓]scroll [1-6]panel [?]help [q]uit".to_string(),
        };
    }

    /// Set target and base branches from resolved refs
    pub fn set_branches(&mut self, target: &str, bases: &[String]) {
        self.target_branch = target.to_string();
        self.base_branches = bases.to_vec();
    }

    /// Initialize checks from config profile
    pub fn init_checks(&mut self, check_names: &[&str]) {
        self.checks_state.entries = check_names
            .iter()
            .map(|name| CheckEntry::new(name))
            .collect();
    }

    /// Update check lifecycle state
    pub fn update_check(&mut self, name: &str, status: CheckLifecycle) {
        if let Some(entry) = self
            .checks_state
            .entries
            .iter_mut()
            .find(|e| e.name == name)
        {
            entry.status = status;
        }
    }

    /// Set check result
    pub fn set_check_result(&mut self, result: &CheckResult) {
        if let Some(entry) = self
            .checks_state
            .entries
            .iter_mut()
            .find(|e| e.name == result.name)
        {
            *entry = CheckEntry::from_result(result);
        }
    }

    /// Populate diffs from analysis
    pub fn set_diffs(&mut self, diffs: &[Diff]) {
        self.diffs_state.files.clear();
        self.diffs_state.total_additions = 0;
        self.diffs_state.total_deletions = 0;
        self.diffs_state.commits_count = 0;

        for diff in diffs {
            self.diffs_state.commits_count += diff.commits.len();
            self.diffs_state.total_additions += diff.stats.additions;
            self.diffs_state.total_deletions += diff.stats.deletions;

            for file in &diff.files {
                use crate::git::FileStatus;

                let kind = match file.status {
                    FileStatus::Added => FileChangeKind::Added,
                    FileStatus::Deleted => FileChangeKind::Deleted,
                    FileStatus::Renamed => FileChangeKind::Renamed,
                    FileStatus::Modified | FileStatus::Copied => FileChangeKind::Modified,
                };
                self.diffs_state.files.push(FileEntry {
                    path: file.path.clone(),
                    kind,
                    additions: file.additions,
                    deletions: file.deletions,
                    base_ref: Some(diff.base_commit_id.clone()),
                    target_ref: Some(diff.target_commit_id.clone()),
                });
            }
        }
    }

    /// Populate artifacts from generated directory
    pub fn set_artifacts(&mut self, dir: &std::path::Path) {
        self.artifacts_state.entries.clear();

        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                let is_dir = path.is_dir();
                let size = if is_dir {
                    0
                } else {
                    std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
                };

                self.artifacts_state.entries.push(ArtifactEntry {
                    name,
                    path,
                    size,
                    is_dir,
                });
            }
        }

        // Sort: directories first, then by name
        self.artifacts_state
            .entries
            .sort_by(|a, b| match (a.is_dir, b.is_dir) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => a.name.cmp(&b.name),
            });
    }

    /// Populate heuristics from analysis
    pub fn set_heuristics(&mut self, result: &HeuristicsResult) {
        if let Some(loctree) = &result.loctree {
            self.heuristics_state.dead_exports_count = loctree.dead_exports.len();
            self.heuristics_state.cycles_count = loctree.cycles.len();
            self.heuristics_state.twins_count = loctree.twins.dead_parrots.len();
        } else {
            // No loctree analysis this run: clear counts from any prior run so
            // the panel never presents stale numbers as the current result.
            self.heuristics_state.dead_exports_count = 0;
            self.heuristics_state.cycles_count = 0;
            self.heuristics_state.twins_count = 0;
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Events
// ─────────────────────────────────────────────────────────────────────────────

/// TUI events for async communication
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// Tick for animations/refresh
    Tick,
    /// Keyboard input
    Key(crossterm::event::KeyEvent),
    /// Check started
    CheckStarted { name: String },
    /// Check completed
    CheckCompleted { result: Box<CheckResult> },
    /// Diffs generated
    DiffsReady { diffs: Vec<Diff> },
    /// Heuristics completed
    HeuristicsReady { result: HeuristicsResult },
    /// Artifacts generated
    ArtifactsReady { dir: PathBuf },
    /// Analysis complete
    AnalysisComplete { report: Report },
    /// Error occurred
    Error { message: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use std::time::Duration;

    fn default_config() -> Config {
        test_config()
    }

    #[test]
    fn test_panel_index() {
        assert_eq!(Panel::Config.index(), 0);
        assert_eq!(Panel::Checks.index(), 1);
        assert_eq!(Panel::Diffs.index(), 2);
        assert_eq!(Panel::Artifacts.index(), 3);
        assert_eq!(Panel::Heuristics.index(), 4);
    }

    #[test]
    fn test_panel_from_index() {
        assert_eq!(Panel::from_index(0), Panel::Config);
        assert_eq!(Panel::from_index(1), Panel::Checks);
        assert_eq!(Panel::from_index(2), Panel::Diffs);
        assert_eq!(Panel::from_index(3), Panel::Artifacts);
        assert_eq!(Panel::from_index(4), Panel::Heuristics);
        assert_eq!(Panel::from_index(5), Panel::State);
        assert_eq!(Panel::from_index(99), Panel::Artifacts); // 99 % 6 == 3
    }

    #[test]
    fn test_panel_label() {
        assert_eq!(Panel::Config.label(), "Config");
        assert_eq!(Panel::Checks.label(), "Checks");
        assert_eq!(Panel::Diffs.label(), "Diffs");
        assert_eq!(Panel::Artifacts.label(), "Artifacts");
        assert_eq!(Panel::Heuristics.label(), "Heuristics");
        assert_eq!(Panel::State.label(), "State");
    }

    #[test]
    fn test_panel_next() {
        assert_eq!(Panel::Config.next(), Panel::Checks);
        assert_eq!(Panel::Checks.next(), Panel::Diffs);
        assert_eq!(Panel::Diffs.next(), Panel::Artifacts);
        assert_eq!(Panel::Artifacts.next(), Panel::Heuristics);
        assert_eq!(Panel::Heuristics.next(), Panel::State);
        assert_eq!(Panel::State.next(), Panel::Config); // wraps
    }

    #[test]
    fn test_panel_prev() {
        assert_eq!(Panel::Config.prev(), Panel::State); // wraps
        assert_eq!(Panel::Checks.prev(), Panel::Config);
        assert_eq!(Panel::Diffs.prev(), Panel::Checks);
        assert_eq!(Panel::Artifacts.prev(), Panel::Diffs);
        assert_eq!(Panel::Heuristics.prev(), Panel::Artifacts);
        assert_eq!(Panel::State.prev(), Panel::Heuristics);
    }

    #[test]
    fn test_check_status_icon() {
        assert_eq!(CheckLifecycle::Pending.icon(), "○");
        assert_eq!(CheckLifecycle::Running.icon(), "◐");
        assert_eq!(CheckLifecycle::Completed.icon(), "●");
        assert_eq!(CheckLifecycle::Warned.icon(), "⚠");
        assert_eq!(CheckLifecycle::Failed.icon(), "✗");
        assert_eq!(CheckLifecycle::Skipped.icon(), "○");
        assert_eq!(CheckLifecycle::Cached.icon(), "●");
    }

    #[test]
    fn test_file_change_kind_icon() {
        assert_eq!(FileChangeKind::Added.icon(), "A");
        assert_eq!(FileChangeKind::Modified.icon(), "M");
        assert_eq!(FileChangeKind::Deleted.icon(), "D");
        assert_eq!(FileChangeKind::Renamed.icon(), "R");
    }

    #[test]
    fn test_check_entry_new() {
        let entry = CheckEntry::new("TypeScript");
        assert_eq!(entry.name, "TypeScript");
        assert_eq!(entry.status, CheckLifecycle::Pending);
        assert!(entry.duration.is_none());
        assert!(entry.output.is_empty());
        assert!(!entry.cached);
    }

    #[test]
    fn test_check_entry_from_result() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let result = CheckResult {
            name: "Clippy".to_string(),
            status: CrateCheckStatus::Passed,
            duration: Duration::from_secs(2),
            output: "All good".to_string(),
            cached: true,
            provenance: None,
        };

        let entry = CheckEntry::from_result(&result);
        assert_eq!(entry.name, "Clippy");
        assert_eq!(entry.status, CheckLifecycle::Cached);
        assert_eq!(entry.duration, Some(Duration::from_secs(2)));
        assert_eq!(entry.output, "All good");
        assert!(entry.cached);
    }

    #[test]
    fn test_check_entry_from_result_failed() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let result = CheckResult {
            name: "Clippy".to_string(),
            status: CrateCheckStatus::Failed,
            duration: Duration::from_secs(1),
            output: "Error".to_string(),
            cached: false,
            provenance: None,
        };

        let entry = CheckEntry::from_result(&result);
        assert_eq!(entry.status, CheckLifecycle::Failed);
    }

    #[test]
    fn test_tui_state_new() {
        let config = default_config();
        let state = TuiState::new(config);

        assert_eq!(state.current_panel, Panel::Config);
        assert!(!state.show_help);
        assert!(state.target_branch.is_empty());
        assert!(state.base_branches.is_empty());
        assert!(!state.running);
        assert!(state.report.is_none());
        assert!(!state.should_quit);
    }

    #[test]
    fn test_tui_state_set_branches() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.set_branches("feature/test", &["main".to_string(), "develop".to_string()]);

        assert_eq!(state.target_branch, "feature/test");
        assert_eq!(
            state.base_branches,
            vec!["main".to_string(), "develop".to_string()]
        );
    }

    #[test]
    fn test_tui_state_init_checks() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.init_checks(&["TypeScript", "Clippy"]);

        assert_eq!(state.checks_state.entries.len(), 2);
        assert_eq!(state.checks_state.entries[0].name, "TypeScript");
        assert_eq!(state.checks_state.entries[1].name, "Clippy");
    }

    #[test]
    fn test_tui_state_update_check() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.init_checks(&["TypeScript"]);
        state.update_check("TypeScript", CheckLifecycle::Running);

        assert_eq!(
            state.checks_state.entries[0].status,
            CheckLifecycle::Running
        );
    }

    #[test]
    fn test_tui_state_update_message_running() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.running = true;
        state.start_time = Some(std::time::Instant::now());
        state.update_message();

        assert!(state.message.contains("Running analysis"));
    }

    #[test]
    fn test_tui_state_update_message_config_panel() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.current_panel = Panel::Config;
        state.update_message();

        assert!(state.message.contains("[r]un"));
        assert!(state.message.contains("[?]help"));
    }

    #[test]
    fn test_tui_state_update_message_checks_panel() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.current_panel = Panel::Checks;
        state.update_message();

        // Checks panel exposes no Enter action; the hint must not promise one.
        assert!(state.message.contains("[↑↓]select"));
        assert!(!state.message.contains("[Enter]"));
    }

    #[test]
    fn test_panel_default() {
        assert_eq!(Panel::default(), Panel::Config);
    }

    #[test]
    fn test_check_status_default() {
        assert_eq!(CheckLifecycle::default(), CheckLifecycle::Pending);
    }

    #[test]
    fn test_tui_state_update_message_diffs_panel() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.current_panel = Panel::Diffs;
        state.update_message();

        assert!(state.message.contains("[Enter]file diff"));
    }

    #[test]
    fn test_tui_state_update_message_artifacts_panel() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.current_panel = Panel::Artifacts;
        state.update_message();

        assert!(state.message.contains("[Enter]open"));
    }

    #[test]
    fn test_tui_state_update_message_heuristics_panel() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.current_panel = Panel::Heuristics;
        state.update_message();

        // Heuristics panel only scrolls sections; no Enter action exists.
        assert!(state.message.contains("[↑↓]select"));
        assert!(!state.message.contains("[Enter]"));
    }

    #[test]
    fn test_check_entry_from_result_warnings() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let result = CheckResult {
            name: "Clippy".to_string(),
            status: CrateCheckStatus::Warnings,
            duration: Duration::from_secs(1),
            output: "Warnings".to_string(),
            cached: false,
            provenance: None,
        };

        let entry = CheckEntry::from_result(&result);
        // A warned check must never collapse to a clean pass in the TUI.
        assert_eq!(entry.status, CheckLifecycle::Warned);
        assert_ne!(entry.status, CheckLifecycle::Completed);
    }

    #[test]
    fn test_check_entry_from_result_skipped() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let result = CheckResult {
            name: "Test".to_string(),
            status: CrateCheckStatus::Skipped,
            duration: Duration::from_secs(0),
            output: String::new(),
            cached: false,
            provenance: None,
        };

        let entry = CheckEntry::from_result(&result);
        assert_eq!(entry.status, CheckLifecycle::Skipped);
    }

    #[test]
    fn test_check_entry_from_result_error() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let result = CheckResult {
            name: "Test".to_string(),
            status: CrateCheckStatus::Error,
            duration: Duration::from_secs(0),
            output: "Error".to_string(),
            cached: false,
            provenance: None,
        };

        let entry = CheckEntry::from_result(&result);
        assert_eq!(entry.status, CheckLifecycle::Failed);
    }

    #[test]
    fn test_file_entry_creation() {
        let entry = FileEntry {
            path: "src/main.rs".to_string(),
            kind: FileChangeKind::Modified,
            additions: 10,
            deletions: 5,
            base_ref: None,
            target_ref: None,
        };
        assert_eq!(entry.path, "src/main.rs");
        assert_eq!(entry.kind, FileChangeKind::Modified);
    }

    #[test]
    fn test_artifact_entry_creation() {
        let entry = ArtifactEntry {
            name: "PR_REVIEW.md".to_string(),
            path: PathBuf::from("/tmp/artifacts/PR_REVIEW.md"),
            size: 1024,
            is_dir: false,
        };
        assert_eq!(entry.name, "PR_REVIEW.md");
        assert!(!entry.is_dir);
    }

    #[test]
    fn test_config_panel_state_default() {
        let state = ConfigPanelState::default();
        assert_eq!(state.scroll_offset, 0);
    }

    #[test]
    fn test_checks_panel_state_default() {
        let state = ChecksPanelState::default();
        assert!(state.entries.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_diffs_panel_state_default() {
        let state = DiffsPanelState::default();
        assert!(state.files.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_artifacts_panel_state_default() {
        let state = ArtifactsPanelState::default();
        assert!(state.entries.is_empty());
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn test_heuristics_panel_state_default() {
        let state = HeuristicsPanelState::default();
        assert_eq!(state.dead_exports_count, 0);
        assert_eq!(state.cycles_count, 0);
    }

    #[test]
    fn test_tui_state_update_check_not_found() {
        let config = default_config();
        let mut state = TuiState::new(config);

        state.init_checks(&["TypeScript"]);
        state.update_check("NonExistent", CheckLifecycle::Running);

        // Should not panic, check status unchanged
        assert_eq!(
            state.checks_state.entries[0].status,
            CheckLifecycle::Pending
        );
    }

    #[test]
    fn test_panel_equality() {
        assert_eq!(Panel::Config, Panel::Config);
        assert_ne!(Panel::Config, Panel::Checks);
    }

    #[test]
    fn test_check_status_equality() {
        assert_eq!(CheckLifecycle::Pending, CheckLifecycle::Pending);
        assert_ne!(CheckLifecycle::Pending, CheckLifecycle::Running);
    }

    #[test]
    fn test_file_change_kind_equality() {
        assert_eq!(FileChangeKind::Added, FileChangeKind::Added);
        assert_ne!(FileChangeKind::Added, FileChangeKind::Modified);
    }

    #[test]
    fn test_tui_state_set_check_result_not_found() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let config = default_config();
        let mut state = TuiState::new(config);
        state.init_checks(&["TypeScript"]);

        let result = CheckResult {
            name: "NonExistent".to_string(),
            status: CrateCheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: String::new(),
            cached: false,
            provenance: None,
        };

        state.set_check_result(&result);
        // Should not panic
        assert_eq!(
            state.checks_state.entries[0].status,
            CheckLifecycle::Pending
        );
    }

    #[test]
    fn test_tui_state_set_check_result() {
        use crate::checks::{CheckResult, CheckStatus as CrateCheckStatus};

        let config = default_config();
        let mut state = TuiState::new(config);
        state.init_checks(&["TypeScript"]);

        let result = CheckResult {
            name: "TypeScript".to_string(),
            status: CrateCheckStatus::Passed,
            duration: Duration::from_secs(1),
            output: "Success".to_string(),
            cached: false,
            provenance: None,
        };

        state.set_check_result(&result);
        assert_eq!(
            state.checks_state.entries[0].status,
            CheckLifecycle::Completed
        );
        assert_eq!(state.checks_state.entries[0].output, "Success");
    }

    #[test]
    fn test_check_entry_clone() {
        let entry = CheckEntry::new("Test");
        let cloned = entry.clone();
        assert_eq!(entry.name, cloned.name);
    }

    #[test]
    fn test_file_entry_clone() {
        let entry = FileEntry {
            path: "test.rs".to_string(),
            kind: FileChangeKind::Added,
            additions: 10,
            deletions: 0,
            base_ref: None,
            target_ref: None,
        };
        let cloned = entry.clone();
        assert_eq!(entry.path, cloned.path);
    }

    #[test]
    fn test_artifact_entry_clone() {
        let entry = ArtifactEntry {
            name: "file.txt".to_string(),
            path: PathBuf::from("/tmp/file.txt"),
            size: 100,
            is_dir: false,
        };
        let cloned = entry.clone();
        assert_eq!(entry.name, cloned.name);
    }

    #[test]
    fn test_tui_state_set_diffs() {
        use crate::git::{Diff, DiffStats, FileChange, FileStatus};

        let config = default_config();
        let mut state = TuiState::new(config);

        let diffs = vec![Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![
                FileChange {
                    path: "src/main.rs".to_string(),
                    status: FileStatus::Modified,
                    additions: 10,
                    deletions: 5,
                },
                FileChange {
                    path: "new.rs".to_string(),
                    status: FileStatus::Added,
                    additions: 50,
                    deletions: 0,
                },
            ],
            stats: DiffStats {
                files_changed: 2,
                additions: 60,
                deletions: 5,
                copied: 0,
            },
            commits: vec![],
        }];

        state.set_diffs(&diffs);

        assert_eq!(state.diffs_state.files.len(), 2);
        assert_eq!(state.diffs_state.total_additions, 60);
        assert_eq!(state.diffs_state.total_deletions, 5);
        assert_eq!(
            state.diffs_state.files[0].base_ref.as_deref(),
            Some("abc123")
        );
        assert_eq!(
            state.diffs_state.files[0].target_ref.as_deref(),
            Some("def456")
        );
    }

    #[test]
    fn test_tui_state_set_diffs_with_deleted() {
        use crate::git::{Diff, DiffStats, FileChange, FileStatus};

        let config = default_config();
        let mut state = TuiState::new(config);

        let diffs = vec![Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![FileChange {
                path: "old.rs".to_string(),
                status: FileStatus::Deleted,
                additions: 0,
                deletions: 100,
            }],
            stats: DiffStats {
                files_changed: 1,
                additions: 0,
                deletions: 100,
                copied: 0,
            },
            commits: vec![],
        }];

        state.set_diffs(&diffs);

        assert_eq!(state.diffs_state.files[0].kind, FileChangeKind::Deleted);
    }

    #[test]
    fn test_tui_state_set_diffs_with_renamed() {
        use crate::git::{Diff, DiffStats, FileChange, FileStatus};

        let config = default_config();
        let mut state = TuiState::new(config);

        let diffs = vec![Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![FileChange {
                path: "renamed.rs".to_string(),
                status: FileStatus::Renamed,
                additions: 0,
                deletions: 0,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        }];

        state.set_diffs(&diffs);

        assert_eq!(state.diffs_state.files[0].kind, FileChangeKind::Renamed);
    }

    #[test]
    fn test_tui_state_set_diffs_with_copied() {
        use crate::git::{Diff, DiffStats, FileChange, FileStatus};

        let config = default_config();
        let mut state = TuiState::new(config);

        let diffs = vec![Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![FileChange {
                path: "copied.rs".to_string(),
                status: FileStatus::Copied,
                additions: 0,
                deletions: 0,
            }],
            stats: DiffStats::default(),
            commits: vec![],
        }];

        state.set_diffs(&diffs);

        assert_eq!(state.diffs_state.files[0].kind, FileChangeKind::Modified);
    }

    #[test]
    fn test_tui_state_set_diffs_with_commits() {
        use crate::git::{CommitInfo, Diff, DiffStats};

        let config = default_config();
        let mut state = TuiState::new(config);

        let diffs = vec![Diff {
            base: "main".to_string(),
            target: "feature".to_string(),
            base_commit_id: "abc123".to_string(),
            target_commit_id: "def456".to_string(),
            files: vec![],
            stats: DiffStats::default(),
            commits: vec![
                CommitInfo {
                    id: "abc".to_string(),
                    short_id: "a".to_string(),
                    author: "Author".to_string(),
                    email: "a@test.com".to_string(),
                    date: "2024-01-01".to_string(),
                    message: "First".to_string(),
                },
                CommitInfo {
                    id: "def".to_string(),
                    short_id: "d".to_string(),
                    author: "Author".to_string(),
                    email: "a@test.com".to_string(),
                    date: "2024-01-02".to_string(),
                    message: "Second".to_string(),
                },
            ],
        }];

        state.set_diffs(&diffs);

        assert_eq!(state.diffs_state.commits_count, 2);
    }

    #[test]
    fn test_select_remote_target_enables_remote_mode() {
        let config = default_config();
        let mut state = TuiState::new(config);
        state.wizard_mode = WizardMode::SelectTarget;
        state.branch_selector.show_local = false;
        state.branch_selector.remote_branches = vec!["origin/feature/remote".to_string()];

        state.select_branch();

        assert_eq!(state.target_branch, "origin/feature/remote");
        assert!(state.config.remote_mode);
        assert_eq!(state.wizard_mode, WizardMode::SelectBase);
    }

    #[test]
    fn test_tui_state_set_heuristics() {
        use crate::heuristics::{
            CycleInfo, DeadExport, DeadParrot, HeuristicsResult, HeuristicsSummary,
            LoctreeAnalysis, TwinsAnalysis,
        };

        let config = default_config();
        let mut state = TuiState::new(config);

        let result = HeuristicsResult {
            loctree: Some(LoctreeAnalysis {
                stats: Default::default(),
                dead_exports: vec![
                    DeadExport {
                        file: "a.rs".to_string(),
                        symbol: "unused".to_string(),
                        line: Some(10),
                        confidence: "high".to_string(),
                    },
                    DeadExport {
                        file: "b.rs".to_string(),
                        symbol: "unused2".to_string(),
                        line: None,
                        confidence: "medium".to_string(),
                    },
                ],
                cycles: vec![CycleInfo {
                    files: vec!["a.rs".to_string(), "b.rs".to_string()],
                    length: 2,
                }],
                twins: TwinsAnalysis {
                    dead_parrots: vec![
                        DeadParrot {
                            file: "c.rs".to_string(),
                            symbol: "dead".to_string(),
                            kind: "function".to_string(),
                            line: 5,
                        },
                        DeadParrot {
                            file: "d.rs".to_string(),
                            symbol: "dead2".to_string(),
                            kind: "const".to_string(),
                            line: 10,
                        },
                        DeadParrot {
                            file: "e.rs".to_string(),
                            symbol: "dead3".to_string(),
                            kind: "type".to_string(),
                            line: 15,
                        },
                    ],
                    exact_twins: vec![],
                    total_symbols: 100,
                },
                available: true,
            }),
            summary: HeuristicsSummary::default(),
            analysis_root: None,
            regression: None,
        };

        state.set_heuristics(&result);

        assert_eq!(state.heuristics_state.dead_exports_count, 2);
        assert_eq!(state.heuristics_state.cycles_count, 1);
        assert_eq!(state.heuristics_state.twins_count, 3);
    }

    #[test]
    fn test_tui_state_set_heuristics_no_loctree() {
        use crate::heuristics::{HeuristicsResult, HeuristicsSummary};

        let config = default_config();
        let mut state = TuiState::new(config);

        // Simulate stale counts left over from a prior run that had loctree.
        state.heuristics_state.dead_exports_count = 7;
        state.heuristics_state.cycles_count = 3;
        state.heuristics_state.twins_count = 5;

        let result = HeuristicsResult {
            loctree: None,
            summary: HeuristicsSummary::default(),
            analysis_root: None,
            regression: None,
        };

        state.set_heuristics(&result);

        // A run without loctree must clear stale counts, not preserve them.
        assert_eq!(state.heuristics_state.dead_exports_count, 0);
        assert_eq!(state.heuristics_state.cycles_count, 0);
        assert_eq!(state.heuristics_state.twins_count, 0);
    }

    #[test]
    fn test_tui_state_set_artifacts() {
        use tempfile::TempDir;

        let config = default_config();
        let mut state = TuiState::new(config);

        // Create a temp directory with some files
        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create a file
        std::fs::write(temp_path.join("test.txt"), "content").unwrap();
        // Create a subdirectory
        std::fs::create_dir(temp_path.join("subdir")).unwrap();

        state.set_artifacts(temp_path);

        assert_eq!(state.artifacts_state.entries.len(), 2);
        // Directories should be first
        assert!(state.artifacts_state.entries[0].is_dir);
        assert_eq!(state.artifacts_state.entries[0].name, "subdir");
        assert!(!state.artifacts_state.entries[1].is_dir);
        assert_eq!(state.artifacts_state.entries[1].name, "test.txt");
    }

    #[test]
    fn test_tui_state_set_artifacts_empty_dir() {
        use tempfile::TempDir;

        let config = default_config();
        let mut state = TuiState::new(config);

        let temp_dir = TempDir::new().unwrap();
        state.set_artifacts(temp_dir.path());

        assert!(state.artifacts_state.entries.is_empty());
    }

    #[test]
    fn test_tui_state_set_artifacts_nonexistent_dir() {
        let config = default_config();
        let mut state = TuiState::new(config);

        let nonexistent = PathBuf::from("/nonexistent/path/12345");
        state.set_artifacts(&nonexistent);

        assert!(state.artifacts_state.entries.is_empty());
    }

    #[test]
    fn test_tui_state_set_artifacts_sorting() {
        use tempfile::TempDir;

        let config = default_config();
        let mut state = TuiState::new(config);

        let temp_dir = TempDir::new().unwrap();
        let temp_path = temp_dir.path();

        // Create files and dirs in non-sorted order
        std::fs::write(temp_path.join("zebra.txt"), "z").unwrap();
        std::fs::create_dir(temp_path.join("beta_dir")).unwrap();
        std::fs::write(temp_path.join("alpha.txt"), "a").unwrap();
        std::fs::create_dir(temp_path.join("alpha_dir")).unwrap();

        state.set_artifacts(temp_path);

        assert_eq!(state.artifacts_state.entries.len(), 4);
        // Directories first, sorted by name
        assert!(state.artifacts_state.entries[0].is_dir);
        assert_eq!(state.artifacts_state.entries[0].name, "alpha_dir");
        assert!(state.artifacts_state.entries[1].is_dir);
        assert_eq!(state.artifacts_state.entries[1].name, "beta_dir");
        // Then files, sorted by name
        assert!(!state.artifacts_state.entries[2].is_dir);
        assert_eq!(state.artifacts_state.entries[2].name, "alpha.txt");
        assert!(!state.artifacts_state.entries[3].is_dir);
        assert_eq!(state.artifacts_state.entries[3].name, "zebra.txt");
    }
}
