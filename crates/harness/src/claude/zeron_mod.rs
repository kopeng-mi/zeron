//! Zeron's bundled Claude Code mod (`assets/claude-mod`). Read-only: at the
//! end of each main turn it logs one `zeron:` JSON line, which arrives on the
//! stream-json stdout as a `system`/`ui_log` frame. The engine installs it
//! into its data dir; runs then load it with `--plugin-dir`.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use serde::Deserialize;
use zeron_proto::{CacheStats, ContextBreakdown};

/// The mod's plugin name, as `ui_log` frames report it.
pub(crate) const PLUGIN: &str = "zeron";

const FILES: &[(&str, &str)] = &[
    (
        ".claude-plugin/plugin.json",
        include_str!("../../assets/claude-mod/.claude-plugin/plugin.json"),
    ),
    (
        "hooks/hooks.json",
        include_str!("../../assets/claude-mod/hooks/hooks.json"),
    ),
    (
        "hooks/register.js",
        include_str!("../../assets/claude-mod/hooks/register.js"),
    ),
];

static DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Write the mod under `data_dir` and load it in later runs, or stop loading
/// it when `enabled` is false.
pub fn install(data_dir: &Path, enabled: bool) -> std::io::Result<()> {
    let mut dir = DIR.lock().unwrap_or_else(PoisonError::into_inner);
    *dir = None;
    if !enabled {
        return Ok(());
    }
    let root = data_dir.join("claude-mod");
    write_files(&root)?;
    *dir = Some(root);
    Ok(())
}

fn write_files(root: &Path) -> std::io::Result<()> {
    for (name, text) in FILES {
        let path = root.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(*text) {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, text)?;
    }
    Ok(())
}

/// Where runs load the mod from, when it's installed.
pub(crate) fn dir() -> Option<PathBuf> {
    DIR.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// One turn's report.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Report {
    pub tokens: Option<u64>,
    pub window: Option<u64>,
    pub cost_micros: Option<u64>,
    pub cache: Option<CacheStats>,
    pub breakdown: Option<ContextBreakdown>,
}

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    context: Option<Context>,
    #[serde(default)]
    cost: Option<Cost>,
    #[serde(default)]
    cache: Option<Cache>,
    #[serde(default)]
    breakdown: Option<ContextBreakdown>,
}

#[derive(Deserialize)]
struct Context {
    tokens: Option<u64>,
    window: Option<u64>,
}

#[derive(Deserialize)]
struct Cost {
    usd: Option<f64>,
}

#[derive(Deserialize)]
struct Cache {
    read: u64,
    write: u64,
    input: u64,
}

/// Parse a `ui_log` text; anything that isn't a `zeron:` report is `None`.
pub(crate) fn parse(text: &str) -> Option<Report> {
    let line: Line = serde_json::from_str(text.strip_prefix("zeron:")?).ok()?;
    let context = line.context;
    Some(Report {
        tokens: context.as_ref().and_then(|c| c.tokens),
        window: context.and_then(|c| c.window).filter(|n| *n > 0),
        cost_micros: line
            .cost
            .and_then(|c| c.usd)
            .filter(|usd| usd.is_finite() && *usd >= 0.0)
            .map(|usd| (usd * 1_000_000.0).round() as u64),
        cache: line.cache.map(|c| CacheStats {
            read: c.read,
            write: c.write,
            input: c.input,
            ttl_secs: None,
            at_ms: 0,
        }),
        breakdown: line.breakdown,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_turn_report() {
        let report = parse(r#"zeron:{"v":1,"context":{"tokens":20346,"window":1000000,"percent":2},"cost":{"usd":0.004076},"cache":{"read":10,"write":20344,"input":2},"breakdown":{"system":1900,"memory":2100,"tools":3000,"mcp":800,"skills":1100,"reminders":600,"messages":10846,"other":5}}"#).unwrap();
        assert_eq!(report.tokens, Some(20346));
        assert_eq!(report.window, Some(1_000_000));
        assert_eq!(report.cost_micros, Some(4076));
        assert_eq!(report.cache.unwrap().write, 20344);
        assert_eq!(report.breakdown.unwrap().messages, 10846);
    }

    #[test]
    fn ignores_other_lines() {
        assert_eq!(parse("hello"), None);
        assert_eq!(parse("zeron:not json"), None);
        assert_eq!(parse("zeron-error:boom"), None);
    }

    #[test]
    fn writes_every_file_of_the_mod() {
        let tmp = tempfile::tempdir().unwrap();
        write_files(tmp.path()).unwrap();
        write_files(tmp.path()).unwrap();
        for (name, text) in FILES {
            assert_eq!(std::fs::read_to_string(tmp.path().join(name)).unwrap(), *text);
        }
    }
}
