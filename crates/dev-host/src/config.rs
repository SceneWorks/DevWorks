//! The optional per-host config file `<root>/dev-host.conf`.
//!
//! Format: one `key = value` per line; blank lines and lines starting with `#` are ignored.
//! Keys read by this version:
//! - `slots_per_key`: build slots allowed per (project, build key). Default
//!   [`DEFAULT_SLOTS_PER_KEY`]; the `DEV_HOST_SLOTS` variable and `--slots` override it.
//! - `age_days`: idle age after which `cleanup` deletes build slots, `t/` entries and campaign
//!   `work/` folders. Default [`DEFAULT_AGE_DAYS`].
//! - `build_budget_gb`, `t_budget_gb`, `cache_budget_gb`, `campaigns_work_budget_gb`,
//!   `docker_budget_gb`: size budgets in GB (10^9 bytes; decimals allowed). An unset budget is
//!   unbounded. Over budget, `cleanup` evicts idle entries from `build/` and `t/` and reports
//!   the others.

use std::collections::BTreeMap;
use std::path::Path;

use crate::Vars;

pub const CONFIG_FILE: &str = "dev-host.conf";
pub const DEFAULT_SLOTS_PER_KEY: u32 = 8;
pub const SLOTS_VAR: &str = "DEV_HOST_SLOTS";
pub const DEFAULT_AGE_DAYS: u64 = 7;

/// Cleanup limits for one host.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Budgets {
    pub age_days: u64,
    pub build: Option<u64>,
    pub t: Option<u64>,
    pub cache: Option<u64>,
    pub campaigns_work: Option<u64>,
    pub docker: Option<u64>,
}

impl Budgets {
    /// Reads the budgets from a parsed config; budgets are returned in bytes.
    pub fn from_config(config: &BTreeMap<String, String>) -> Result<Self, String> {
        let gb = |key: &str| -> Result<Option<u64>, String> {
            let Some(raw) = config.get(key) else {
                return Ok(None);
            };
            match raw.parse::<f64>() {
                Ok(v) if v.is_finite() && v >= 0.0 => Ok(Some((v * 1e9) as u64)),
                _ => Err(format!("{key}={raw} is not a size in GB")),
            }
        };
        let age_days = match config.get("age_days") {
            None => DEFAULT_AGE_DAYS,
            Some(raw) => match raw.parse::<u64>() {
                Ok(n) if n > 0 => n,
                _ => return Err(format!("age_days={raw} is not a whole number of days >= 1")),
            },
        };
        Ok(Self {
            age_days,
            build: gb("build_budget_gb")?,
            t: gb("t_budget_gb")?,
            cache: gb("cache_budget_gb")?,
            campaigns_work: gb("campaigns_work_budget_gb")?,
            docker: gb("docker_budget_gb")?,
        })
    }
}

/// Reads `<root>/dev-host.conf`; a missing file is an empty config.
pub fn read(root: &Path) -> Result<BTreeMap<String, String>, String> {
    let path = root.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse(text: &str) -> Result<BTreeMap<String, String>, String> {
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", n + 1));
        };
        out.insert(key.trim().to_owned(), value.trim().to_owned());
    }
    Ok(out)
}

/// The slot cap: flag, then `DEV_HOST_SLOTS`, then the config file, then the default.
pub fn slots_per_key(
    flag: Option<u32>,
    vars: &Vars,
    config: &BTreeMap<String, String>,
) -> Result<u32, String> {
    let (source, raw) = if let Some(n) = flag {
        return positive(n, "--slots");
    } else if let Some(v) = vars.get(SLOTS_VAR) {
        (SLOTS_VAR, v)
    } else if let Some(v) = config.get("slots_per_key") {
        ("slots_per_key", v.as_str())
    } else {
        return Ok(DEFAULT_SLOTS_PER_KEY);
    };
    let n = raw
        .parse::<u32>()
        .map_err(|_| format!("{source}={raw} is not a whole number"))?;
    positive(n, source)
}

fn positive(n: u32, source: &str) -> Result<u32, String> {
    if n == 0 {
        Err(format!("{source} must be at least 1"))
    } else {
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_layers_slot_cap() {
        let config = parse("# host budgets\nslots_per_key = 4\n\n").unwrap();
        assert_eq!(slots_per_key(None, &Vars::default(), &config), Ok(4));
        let vars: Vars = [(SLOTS_VAR, "6")].into_iter().collect();
        assert_eq!(slots_per_key(None, &vars, &config), Ok(6));
        assert_eq!(slots_per_key(Some(2), &vars, &config), Ok(2));
        assert_eq!(
            slots_per_key(None, &Vars::default(), &BTreeMap::new()),
            Ok(DEFAULT_SLOTS_PER_KEY)
        );
        assert!(slots_per_key(Some(0), &vars, &config).is_err());
        assert!(parse("no equals sign").is_err());
    }

    #[test]
    fn parses_budgets_and_age() {
        let config = parse("build_budget_gb = 300\nt_budget_gb=0.5\nage_days = 3\n").unwrap();
        let b = Budgets::from_config(&config).unwrap();
        assert_eq!(b.build, Some(300_000_000_000));
        assert_eq!(b.t, Some(500_000_000));
        assert_eq!(b.cache, None);
        assert_eq!(b.age_days, 3);
        let d = Budgets::from_config(&BTreeMap::new()).unwrap();
        assert_eq!(d.age_days, DEFAULT_AGE_DAYS);
        assert!(Budgets::from_config(&parse("docker_budget_gb = lots").unwrap()).is_err());
        assert!(Budgets::from_config(&parse("age_days = 0").unwrap()).is_err());
    }
}
