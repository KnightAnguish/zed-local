//! Harness v2 scaffolding.
//!
//! This module hosts the OpenCode-inspired pieces of the native agent harness:
//! context accounting, compaction planning, background shell jobs, and a
//! cache-stable instruction set. It is intentionally inert in phase 0: the
//! legacy `Thread` code still drives every turn and `HarnessMode::Legacy` is
//! the default.

pub mod context;
pub mod settings;
pub mod shell;

pub use context::{
    ContextAnchor, ContextEstimate, ContextLedger, ContextReserve, MeasuredUsage,
    estimate_text_tokens,
};
pub use settings::{CodeModeSettings, CompactionSettings, HarnessMode, HarnessSettings};
pub use shell::{ShellJob, ShellJobId, ShellJobRegistry, ShellJobStatus};

use serde::{Deserialize, Serialize};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

/// A thread-local harness state holder.
///
/// Phase 0 only carries settings, the context ledger, and the background-shell
/// registry. Later phases add the compaction planner and instruction set here;
/// nothing in `Thread` consults it while the mode is [`HarnessMode::Legacy`].
#[derive(Clone, Debug, Default)]
pub struct Harness {
    settings: HarnessSettings,
    context: ContextLedger,
    shell_jobs: ShellJobRegistry,
    loaded_rules: HashSet<PathBuf>,
}

impl Harness {
    pub fn new(settings: HarnessSettings) -> Self {
        Self {
            settings,
            context: ContextLedger::new(),
            shell_jobs: ShellJobRegistry::new(),
            loaded_rules: HashSet::new(),
        }
    }

    pub fn settings(&self) -> &HarnessSettings {
        &self.settings
    }

    pub fn set_settings(&mut self, settings: HarnessSettings) {
        self.settings = settings;
    }

    pub fn mode(&self) -> HarnessMode {
        self.settings.mode
    }

    pub fn context(&self) -> &ContextLedger {
        &self.context
    }

    pub fn context_mut(&mut self) -> &mut ContextLedger {
        &mut self.context
    }

    pub fn shell_jobs(&self) -> &ShellJobRegistry {
        &self.shell_jobs
    }

    pub fn shell_jobs_mut(&mut self) -> &mut ShellJobRegistry {
        &mut self.shell_jobs
    }

    /// Marks a rules file as already injected into this thread. Returns
    /// `false` when the file had already been loaded.
    pub fn mark_rule_loaded(&mut self, path: PathBuf) -> bool {
        self.loaded_rules.insert(path)
    }

    pub fn has_loaded_rule(&self, path: &Path) -> bool {
        self.loaded_rules.contains(path)
    }
}

/// Events emitted by the harness for the UI and telemetry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HarnessEvent {
    /// The current thread's context estimate changed.
    ContextEstimated(ContextEstimate),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_defaults_to_legacy_and_an_empty_context() {
        let harness = Harness::default();
        assert_eq!(harness.mode(), HarnessMode::Legacy);
        assert!(harness.context().estimate().is_empty());
        assert!(harness.shell_jobs().is_empty());
    }

    #[test]
    fn harness_mode_follows_settings() {
        let mut harness = Harness::default();
        harness.set_settings(HarnessSettings {
            mode: HarnessMode::V2,
            ..Default::default()
        });
        assert_eq!(harness.mode(), HarnessMode::V2);
    }

    #[test]
    fn harness_remembers_loaded_rules() {
        let mut harness = Harness::default();
        let path = PathBuf::from("/project/src/AGENTS.md");
        assert!(harness.mark_rule_loaded(path.clone()));
        assert!(!harness.mark_rule_loaded(path.clone()));
        assert!(harness.has_loaded_rule(&path));
    }
}
