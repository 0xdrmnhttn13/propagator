//! Config: `propagator.toml` (sources + store path) and `topics.toml`
//! (registry fallback for dynamic topics).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    #[serde(default)]
    pub store: StoreCfg,
    #[serde(default)]
    pub sources: Vec<SourceCfg>,
    /// Optional path to `topics.toml`; defaults to `topics.toml` next to config.
    pub topics: Option<PathBuf>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StoreCfg {
    pub path: PathBuf,
}

impl Default for StoreCfg {
    fn default() -> Self {
        Self {
            path: PathBuf::from(".propagator/store.bin"),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SourceCfg {
    pub kind: SourceKind,
    pub path: PathBuf,
    /// Logical service name; becomes the `Service` node + `Owns` edges.
    pub service: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// `*.sql` — procedure/function definitions (edges come from oracle_dd).
    Sql,
    /// `*.go` / `*.rs` / `*.cpp` — auto-detect per extension; MQ + Invokes.
    Code,
}

impl Config {
    /// Load + expand `~/` in all paths. Relative paths (store, sources, topics)
    /// resolve against the config file's directory — not the cwd — so a shared
    /// `~/work/propagator.toml` behaves the same from any repo underneath it.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read config {}: {e}", path.display()))?;
        let mut cfg: Config = toml::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("parse config {}: {e}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        cfg.store.path = anchor(base, &expand(&cfg.store.path));
        for s in &mut cfg.sources {
            s.path = anchor(base, &expand(&s.path));
        }
        // topics.toml defaults to sitting next to the config.
        cfg.topics = Some(match &cfg.topics {
            Some(p) => anchor(base, &expand(p)),
            None => base.join("topics.toml"),
        });
        Ok(cfg)
    }

    /// Load an explicit config path (expanding a leading `~/`). Used by
    /// `serve --config` so the MCP server binds to a fixed corpus regardless of
    /// the launching session's cwd (cwd-based `discover` picks whatever
    /// `propagator.toml` sits above the session — often the wrong one).
    pub fn load_explicit(path: &Path) -> anyhow::Result<(Self, PathBuf)> {
        let resolved = expand(path);
        let cfg = Self::load(&resolved)?;
        Ok((cfg, resolved))
    }

    /// Default config search: `propagator.toml` in CWD or parents.
    pub fn discover() -> anyhow::Result<(Self, PathBuf)> {
        let name = "propagator.toml";
        let mut dir = std::env::current_dir()?;
        loop {
            let candidate = dir.join(name);
            if candidate.exists() {
                let cfg = Self::load(&candidate)?;
                return Ok((cfg, candidate));
            }
            match dir.parent() {
                Some(parent) => dir = parent.to_path_buf(),
                None => anyhow::bail!("propagator.toml not found; run `propagator init`"),
            }
        }
    }
}

/// Registry entry: service → publishes/consumes topic lists.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TopicsRegistry {
    #[serde(default)]
    pub services: std::collections::BTreeMap<String, TopicEdges>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TopicEdges {
    #[serde(default)]
    pub publishes: Vec<String>,
    #[serde(default)]
    pub consumes: Vec<String>,
    /// Optional per-topic producer-provenance: `topic name → tables that feed
    /// its payload`. Declared as `[svc.provenance]` in `topics.toml`. Consumed
    /// by `sync.rs` to annotate the `Publishes` edge so impact BFS can tell a
    /// real SQL→MQ data path from the promiscuous service-hub bridge.
    #[serde(default)]
    pub provenance: std::collections::BTreeMap<String, Vec<String>>,
}

impl TopicsRegistry {
    /// Load registry; returns empty if file absent (`topics.toml` is optional).
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("read {}: {e}", path.display()))?;
        // `[service]` table with `publishes`/`consumes` arrays.
        let table: toml::Table =
            toml::from_str(&raw).map_err(|e| anyhow::anyhow!("parse {}: {e}", path.display()))?;
        let mut services = std::collections::BTreeMap::new();
        for (svc, v) in table {
            let Some(map) = v.as_table() else { continue };
            let publishes = map
                .get("publishes")
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let consumes = map
                .get("consumes")
                .and_then(toml::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            // `[svc.provenance]` sub-table: topic → [feeding tables].
            let provenance = map
                .get("provenance")
                .and_then(toml::Value::as_table)
                .map(|t| {
                    t.iter()
                        .map(|(topic, v)| {
                            let tables = v
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(str::to_owned))
                                        .collect()
                                })
                                .unwrap_or_default();
                            (topic.clone(), tables)
                        })
                        .collect()
                })
                .unwrap_or_default();
            services.insert(
                svc,
                TopicEdges {
                    publishes,
                    consumes,
                    provenance,
                },
            );
        }
        Ok(Self { services })
    }
}

/// Anchor a relative path to `base`; absolute paths pass through.
fn anchor(base: &Path, p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

/// Expand a leading `~/` to the home dir. Leaves other paths untouched.
fn expand(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs_home() {
            return home.join(rest);
        }
    }
    p.to_path_buf()
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_anchor_to_config_dir() {
        let dir = std::env::temp_dir().join(format!("prop_cfg_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_path = dir.join("propagator.toml");
        std::fs::write(
            &cfg_path,
            r#"
[store]
path = ".propagator/store.bin"

[[sources]]
kind = "code"
path = "svc-a"
service = "svc-a"

[[sources]]
kind = "sql"
path = "/abs/sql"
service = "oms"
"#,
        )
        .unwrap();
        let cfg = Config::load(&cfg_path).unwrap();
        assert_eq!(cfg.store.path, dir.join(".propagator/store.bin"));
        assert_eq!(cfg.sources[0].path, dir.join("svc-a"));
        assert_eq!(cfg.sources[1].path, PathBuf::from("/abs/sql"));
        // topics defaults next to the config file.
        assert_eq!(
            cfg.topics.as_deref(),
            Some(dir.join("topics.toml").as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
