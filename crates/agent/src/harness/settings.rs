use serde::{Deserialize, Serialize};

/// Selects which native-agent harness drives a thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessMode {
    /// Existing Zed behavior. This remains the default while v2 is built out.
    #[default]
    Legacy,
    /// OpenCode-inspired context, shell, and instruction layers.
    V2,
}

/// Settings for the native-agent harness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HarnessSettings {
    pub mode: HarnessMode,
    /// Allow the `terminal` tool to start commands in the background.
    pub background_shell: bool,
    /// Maximum number of background shell jobs per thread.
    pub max_background_jobs: usize,
    pub compaction: CompactionSettings,
    pub code_mode: CodeModeSettings,
}

impl Default for HarnessSettings {
    fn default() -> Self {
        Self {
            mode: HarnessMode::Legacy,
            background_shell: true,
            max_background_jobs: 4,
            compaction: CompactionSettings::default(),
            code_mode: CodeModeSettings::default(),
        }
    }
}

impl HarnessSettings {
    pub fn is_v2(&self) -> bool {
        self.mode == HarnessMode::V2
    }
}

/// Context-compaction policy for the v2 harness.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Summarize the cached prefix in the background before compaction is due.
    pub precompute: bool,
    /// Fraction of the usable context window that triggers compaction.
    pub threshold: f64,
    /// Fraction of the usable context window that starts a background summary.
    pub precompute_threshold: f64,
    /// Number of recent tokens kept verbatim next to the summary.
    pub keep_tokens: usize,
    /// Prefer a provider-native compaction endpoint when available.
    pub prefer_native: bool,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            precompute: false,
            threshold: 0.80,
            precompute_threshold: 0.70,
            keep_tokens: 15_000,
            prefer_native: true,
        }
    }
}

/// Tool-catalog / Code Mode policy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CodeModeSettings {
    pub enabled: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_keep_the_legacy_harness() {
        let settings = HarnessSettings::default();
        assert_eq!(settings.mode, HarnessMode::Legacy);
        assert!(!settings.is_v2());
        assert!(settings.background_shell);
        assert_eq!(settings.max_background_jobs, 4);
        assert!(settings.compaction.enabled);
        assert!(!settings.compaction.precompute);
        assert_eq!(settings.compaction.threshold, 0.80);
        assert_eq!(settings.compaction.precompute_threshold, 0.70);
        assert_eq!(settings.compaction.keep_tokens, 15_000);
        assert!(settings.compaction.prefer_native);
        assert!(!settings.code_mode.enabled);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let settings = HarnessSettings {
            mode: HarnessMode::V2,
            background_shell: false,
            max_background_jobs: 7,
            compaction: CompactionSettings {
                enabled: false,
                precompute: true,
                threshold: 0.9,
                precompute_threshold: 0.75,
                keep_tokens: 8_000,
                prefer_native: false,
            },
            code_mode: CodeModeSettings { enabled: true },
        };

        let json = serde_json::to_string(&settings).expect("serialize harness settings");
        let parsed: HarnessSettings =
            serde_json::from_str(&json).expect("deserialize harness settings");
        assert_eq!(parsed, settings);
    }

    #[test]
    fn missing_fields_use_defaults() {
        let parsed: HarnessSettings =
            serde_json::from_str(r#"{"mode":"v2"}"#).expect("deserialize partial settings");
        assert_eq!(parsed.mode, HarnessMode::V2);
        assert_eq!(parsed.compaction, CompactionSettings::default());
        assert_eq!(parsed.code_mode, CodeModeSettings::default());
    }
}
