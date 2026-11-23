//! The optional config file `<root>/dev-host.conf`, read by every host that uses the root.
//!
//! Format: one `key = value` per line; blank lines and lines starting with `#` are ignored.
//! A key given twice keeps its last value, except `required_mount`, which may repeat.
//! Keys read by this version:
//! - `slots_per_key`: build slots allowed per (project, build key). Default
//!   [`DEFAULT_SLOTS_PER_KEY`]; the `DEV_HOST_SLOTS` variable and `--slots` override it.
//! - `age_days`: idle age after which `cleanup` deletes build slots, `t/` entries and campaign
//!   `work/` folders. Default [`DEFAULT_AGE_DAYS`].
//! - `build_budget_gb`, `t_budget_gb`, `cache_budget_gb`, `campaigns_work_budget_gb`,
//!   `docker_budget_gb`: size budgets in GB (10^9 bytes; decimals allowed). An unset budget is
//!   unbounded. Each host applies them to its own `hosts/<host>/` folder: over budget,
//!   `cleanup` evicts idle entries from its `build/` and `t/` and reports the others. The
//!   shared `cache/` is measured once against `cache_budget_gb`, never cleaned.
//! - `required_mount = <absolute path>`: a share this host needs besides the root's own
//!   volume (on the Macs, the share holding the Hugging Face hub cache). One line per share.
//!   `dev-host check` fails, naming the path, unless each is a mounted volume: on Unix a mount
//!   point whose device differs from `/`'s, on Windows a fixed or remote volume. Paths live
//!   only here, never in dev-host's source.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::Vars;

pub const CONFIG_FILE: &str = "dev-host.conf";
pub const DEFAULT_SLOTS_PER_KEY: u32 = 8;
pub const SLOTS_VAR: &str = "DEV_HOST_SLOTS";
pub const DEFAULT_AGE_DAYS: u64 = 7;
/// The one key that may repeat.
pub const REQUIRED_MOUNT: &str = "required_mount";

/// A parsed `dev-host.conf`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Config {
    /// Every single-valued key (the last value when one repeats).
    pub values: BTreeMap<String, String>,
    /// Each `required_mount` line, in file order.
    pub required_mounts: Vec<PathBuf>,
}

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
pub fn read(root: &Path) -> Result<Config, String> {
    let path = root.join(CONFIG_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn parse(text: &str) -> Result<Config, String> {
    let mut out = Config::default();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("line {}: expected key = value", n + 1));
        };
        let (key, value) = (key.trim(), value.trim());
        if key == REQUIRED_MOUNT {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!(
                    "line {}: {REQUIRED_MOUNT} = {value} is not an absolute path",
                    n + 1
                ));
            }
            out.required_mounts.push(path);
        } else {
            out.values.insert(key.to_owned(), value.to_owned());
        }
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
        let config = parse("# host budgets\nslots_per_key = 4\n\n")
            .unwrap()
            .values;
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
        let config = parse("build_budget_gb = 300\nt_budget_gb=0.5\nage_days = 3\n")
            .unwrap()
            .values;
        let b = Budgets::from_config(&config).unwrap();
        assert_eq!(b.build, Some(300_000_000_000));
        assert_eq!(b.t, Some(500_000_000));
        assert_eq!(b.cache, None);
        assert_eq!(b.age_days, 3);
        let d = Budgets::from_config(&BTreeMap::new()).unwrap();
        assert_eq!(d.age_days, DEFAULT_AGE_DAYS);
        assert!(Budgets::from_config(&parse("docker_budget_gb = lots").unwrap().values).is_err());
        assert!(Budgets::from_config(&parse("age_days = 0").unwrap().values).is_err());
    }

    #[test]
    fn required_mount_repeats_and_must_be_absolute() {
        let abs = |name: &str| {
            if cfg!(windows) {
                format!(r"M:\{name}")
            } else {
                format!("/mnt/{name}")
            }
        };
        let text = format!(
            "required_mount = {}\nage_days = 3\nrequired_mount={}\n",
            abs("models"),
            abs("work")
        );
        let config = parse(&text).unwrap();
        assert_eq!(
            config.required_mounts,
            vec![PathBuf::from(abs("models")), PathBuf::from(abs("work"))]
        );
        assert!(!config.values.contains_key(REQUIRED_MOUNT));
        assert_eq!(config.values.get("age_days").map(String::as_str), Some("3"));
        let err = parse("required_mount = relative/dir").unwrap_err();
        assert!(err.contains("not an absolute path"), "{err}");
        assert!(parse("required_mount =").is_err());
    }
}
