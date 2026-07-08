//! Configuration for genesoxide.
//!
//! Loads settings from a TOML file (`genesoxide.toml`) with sensible defaults.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Top-level configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GenesisConfig {
    /// Desktop frontend settings.
    #[serde(default)]
    pub desktop: DesktopConfig,
    /// Time-travel rewind settings.
    #[serde(default)]
    pub rewind: RewindConfig,
    /// Named ROM paths.
    #[serde(default)]
    pub roms: std::collections::HashMap<String, String>,
}

/// Time-travel rewind configuration (TOML section `[rewind]`).
///
/// This mirrors `genesoxide_core::RewindConfig` but is defined locally so the
/// config crate does not depend on the core crate. The desktop frontend maps
/// it into the core type when sending `Command::SetRewindConfig`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindConfig {
    /// Whether rewind recording is active.
    #[serde(default = "default_rewind_enabled")]
    pub enabled: bool,
    /// How many seconds of history to retain.
    #[serde(default = "default_max_history_seconds")]
    pub max_history_seconds: u32,
    /// Fixed keyframe promotion interval, in frames.
    #[serde(default = "default_keyframe_base_interval")]
    pub keyframe_base_interval: u64,
    /// Delta size (bytes) above which a spike keyframe may be promoted.
    #[serde(default = "default_delta_spike_threshold")]
    pub delta_spike_threshold: u32,
}

impl Default for RewindConfig {
    fn default() -> Self {
        Self {
            enabled: default_rewind_enabled(),
            max_history_seconds: default_max_history_seconds(),
            keyframe_base_interval: default_keyframe_base_interval(),
            delta_spike_threshold: default_delta_spike_threshold(),
        }
    }
}

fn default_rewind_enabled() -> bool {
    true
}

fn default_max_history_seconds() -> u32 {
    30
}

fn default_keyframe_base_interval() -> u64 {
    60
}

fn default_delta_spike_threshold() -> u32 {
    2048
}

/// Desktop frontend configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DesktopConfig {
    /// Path to the ROM file.
    #[serde(default)]
    pub rom_path: String,
    /// Window scale factor.
    #[serde(default = "default_scale")]
    pub window_scale: u32,
    /// Enable audio output.
    #[serde(default)]
    pub audio_enabled: bool,
    /// Step mode: "frame", "cpu", or "scanline".
    #[serde(default = "default_step_mode")]
    pub step_mode: String,
}

impl Default for DesktopConfig {
    fn default() -> Self {
        Self {
            rom_path: String::new(),
            window_scale: default_scale(),
            audio_enabled: false,
            step_mode: default_step_mode(),
        }
    }
}

fn default_scale() -> u32 {
    3
}

fn default_step_mode() -> String {
    "frame".to_string()
}

impl GenesisConfig {
    /// Loads config from a file, falling back to defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if the file exists but cannot be parsed.
    pub fn load(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        if path.exists() {
            let content = std::fs::read_to_string(path)?;
            let config: GenesisConfig = toml::from_str(&content)?;
            Ok(config)
        } else {
            Ok(Self::default())
        }
    }

    /// Loads from the default path (`genesoxide.toml`), or returns defaults.
    #[must_use]
    pub fn load_or_default() -> Self {
        Self::load(Path::new("genesoxide.toml")).unwrap_or_default()
    }

    /// Resolves a ROM path from a name or direct path.
    #[must_use]
    pub fn resolve_rom(&self, name_or_path: &str) -> PathBuf {
        if let Some(path) = self.roms.get(name_or_path) {
            PathBuf::from(path)
        } else {
            PathBuf::from(name_or_path)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = GenesisConfig::default();
        assert_eq!(config.desktop.window_scale, 3);
        assert!(!config.desktop.audio_enabled);
        assert_eq!(config.desktop.step_mode, "frame");
    }

    #[test]
    fn parse_toml() {
        let toml = r#"
            [desktop]
            rom_path = "sonic.bin"
            window_scale = 4
            audio_enabled = true

            [roms]
            sonic = "/path/to/sonic.bin"
        "#;
        let config: GenesisConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.desktop.window_scale, 4);
        assert!(config.desktop.audio_enabled);
        assert_eq!(config.roms["sonic"], "/path/to/sonic.bin");
    }

    #[test]
    fn default_rewind_config() {
        let config = GenesisConfig::default();
        assert!(config.rewind.enabled);
        assert_eq!(config.rewind.max_history_seconds, 30);
        assert_eq!(config.rewind.keyframe_base_interval, 60);
        assert_eq!(config.rewind.delta_spike_threshold, 2048);
    }

    #[test]
    fn parse_rewind_toml() {
        let toml = r#"
            [rewind]
            enabled = false
            max_history_seconds = 10
        "#;
        let config: GenesisConfig = toml::from_str(toml).unwrap();
        assert!(!config.rewind.enabled);
        assert_eq!(config.rewind.max_history_seconds, 10);
        // Unspecified fields fall back to defaults.
        assert_eq!(config.rewind.keyframe_base_interval, 60);
    }

    #[test]
    fn resolve_named_rom() {
        let mut config = GenesisConfig::default();
        config.roms.insert("sonic".into(), "/roms/sonic.bin".into());

        assert_eq!(
            config.resolve_rom("sonic"),
            PathBuf::from("/roms/sonic.bin")
        );
        assert_eq!(config.resolve_rom("other.bin"), PathBuf::from("other.bin"));
    }
}
