//! `~/.berth/berth.toml`: tags, groups, layouts and fan-out sets, and the
//! accent each program's status is drawn in (D53).
//!
//! This file is Berth's own; unlike ssh config it may be written back. It is
//! intended to be committed to git, which is the only sync mechanism Berth has.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const CURRENT_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BerthConfig {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "host")]
    pub hosts: Vec<HostEntry>,
    #[serde(default, rename = "layout")]
    pub layouts: Vec<Layout>,
    #[serde(default, rename = "fanout")]
    pub fanouts: Vec<Fanout>,
    /// A program's status accent, by the name it runs under, as `#rrggbb`:
    /// over Berth's own for that program, or for one it has none for.
    #[serde(default, rename = "accent", skip_serializing_if = "BTreeMap::is_empty")]
    pub accents: BTreeMap<String, String>,
}

fn default_version() -> u32 {
    CURRENT_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default = "default_font")]
    pub font: String,
    #[serde(default = "default_font_size")]
    pub font_size: f32,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default = "default_retention_days")]
    pub timeline_retention_days: u32,
    #[serde(default = "default_max_gb")]
    pub timeline_max_gb: f64,
    #[serde(default)]
    pub predictive_echo: PredictiveEcho,
}

fn default_font() -> String {
    "JetBrains Mono".to_string()
}
fn default_font_size() -> f32 {
    13.0
}
fn default_retention_days() -> u32 {
    30
}
fn default_max_gb() -> f64 {
    2.0
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font: default_font(),
            font_size: default_font_size(),
            theme: Theme::default(),
            timeline_retention_days: default_retention_days(),
            timeline_max_gb: default_max_gb(),
            predictive_echo: PredictiveEcho::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PredictiveEcho {
    #[default]
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostEntry {
    pub alias: String,
    /// Required only for hosts absent from ssh config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    pub name: String,
    #[serde(default)]
    pub panes: Vec<Pane>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pane {
    pub host: String,
    #[serde(default = "default_session")]
    pub session: String,
}

fn default_session() -> String {
    "main".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fanout {
    pub name: String,
    #[serde(default)]
    pub hosts: Vec<String>,
}

impl BerthConfig {
    pub fn data_dir() -> Option<PathBuf> {
        dirs::home_dir().map(|h| h.join(".berth"))
    }

    pub fn default_path() -> Option<PathBuf> {
        Self::data_dir().map(|d| d.join("berth.toml"))
    }

    /// Where it is read from: `BERTH_CONFIG`, else `~/.berth/berth.toml`.
    pub fn path() -> Option<PathBuf> {
        crate::env::config().or_else(Self::default_path)
    }

    /// A missing file yields defaults, so a fresh install still starts.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self {
                version: CURRENT_VERSION,
                ..Default::default()
            });
        }
        Self::load(path)
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self> {
        let config: BerthConfig = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.version != CURRENT_VERSION {
            return Err(Error::Config(format!(
                "unsupported version {}, expected {CURRENT_VERSION}",
                self.version
            )));
        }
        for host in &self.hosts {
            if self.hosts.iter().filter(|h| h.alias == host.alias).count() > 1 {
                return Err(Error::Config(format!(
                    "duplicate host alias {}",
                    host.alias
                )));
            }
        }
        for (name, colour) in &self.accents {
            if parse_colour(colour).is_none() {
                return Err(Error::Config(format!(
                    "the accent for {name} is not a #rrggbb colour: {colour}"
                )));
            }
        }
        for fanout in &self.fanouts {
            for alias in &fanout.hosts {
                if !self.hosts.iter().any(|h| &h.alias == alias) {
                    // Fan-out may reference an ssh-config-only host, so this is
                    // a warning at resolve time rather than an error here.
                    continue;
                }
            }
        }
        Ok(())
    }

    pub fn host(&self, alias: &str) -> Option<&HostEntry> {
        self.hosts.iter().find(|h| h.alias == alias)
    }

    pub fn layout(&self, name: &str) -> Option<&Layout> {
        self.layouts.iter().find(|l| l.name == name)
    }

    pub fn fanout(&self, name: &str) -> Option<&Fanout> {
        self.fanouts.iter().find(|f| f.name == name)
    }

    /// The accent given for a program, by its name, case aside.
    pub fn accent(&self, program: &str) -> Option<[u8; 3]> {
        self.accents
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(program))
            .and_then(|(_, colour)| parse_colour(colour))
    }
}

/// `#rrggbb` as its three bytes.
fn parse_colour(colour: &str) -> Option<[u8; 3]> {
    let hex = colour.trim().strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
version = 1

[settings]
font = "JetBrains Mono"
font_size = 13
theme = "system"
timeline_retention_days = 30
timeline_max_gb = 2
predictive_echo = "auto"

[[host]]
alias = "web-1"
tags = ["prod"]
group = "web"
color = "red"

[[host]]
alias = "scratch"
hostname = "203.0.113.10"
user = "root"
tags = ["dev"]

[[layout]]
name = "morning"
panes = [
  { host = "web-1", session = "main" },
]

[[fanout]]
name = "web"
hosts = ["web-1"]
"#;

    #[test]
    fn parses_the_documented_sample() {
        let c = BerthConfig::parse(SAMPLE).unwrap();
        assert_eq!(c.version, 1);
        assert_eq!(c.settings.font_size, 13.0);
        assert_eq!(c.settings.theme, Theme::System);
        assert_eq!(c.settings.predictive_echo, PredictiveEcho::Auto);
        assert_eq!(c.hosts.len(), 2);
        assert_eq!(c.host("web-1").unwrap().tags, vec!["prod"]);
        assert_eq!(c.host("scratch").unwrap().user.as_deref(), Some("root"));
        assert_eq!(c.layout("morning").unwrap().panes[0].host, "web-1");
        assert_eq!(c.fanout("web").unwrap().hosts, vec!["web-1"]);
    }

    #[test]
    fn empty_file_yields_defaults() {
        let c = BerthConfig::parse("version = 1").unwrap();
        assert_eq!(c.settings.font, "JetBrains Mono");
        assert_eq!(c.settings.timeline_retention_days, 30);
        assert!(c.hosts.is_empty());
    }

    #[test]
    fn pane_session_defaults_to_main() {
        let c =
            BerthConfig::parse("version = 1\n[[layout]]\nname = \"l\"\npanes = [{ host = \"a\" }]")
                .unwrap();
        assert_eq!(c.layouts[0].panes[0].session, "main");
    }

    #[test]
    fn accents_are_read_by_program_name() {
        let c = BerthConfig::parse(
            "version = 1\n[accent]\nclaude = \"#112233\"\nmy-tool = \"#AbCdEf\"\n",
        )
        .unwrap();
        assert_eq!(c.accent("claude"), Some([0x11, 0x22, 0x33]));
        assert_eq!(c.accent("Claude"), Some([0x11, 0x22, 0x33]));
        assert_eq!(c.accent("my-tool"), Some([0xab, 0xcd, 0xef]));
        assert_eq!(c.accent("codex"), None);
        for bad in ["red", "#12345", "#1234567", "112233", "#gg0000"] {
            let text = format!("version = 1\n[accent]\nclaude = \"{bad}\"\n");
            assert!(BerthConfig::parse(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_wrong_version() {
        assert!(BerthConfig::parse("version = 99").is_err());
    }

    #[test]
    fn rejects_duplicate_alias() {
        let text = "version = 1\n[[host]]\nalias = \"a\"\n[[host]]\nalias = \"a\"\n";
        assert!(BerthConfig::parse(text).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        assert!(BerthConfig::parse("version = 1\n[settings]\nnope = 1").is_err());
    }

    #[test]
    fn round_trips_through_serialisation() {
        let c = BerthConfig::parse(SAMPLE).unwrap();
        let text = toml::to_string(&c).unwrap();
        let again = BerthConfig::parse(&text).unwrap();
        assert_eq!(again.hosts.len(), c.hosts.len());
        assert_eq!(again.settings.font, c.settings.font);
    }
}
