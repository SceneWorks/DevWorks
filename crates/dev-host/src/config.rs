//! The optional per-host config file `<root>/dev-host.conf`.
//!
//! Format: one `key = value` per line; blank lines and lines starting with `#` are ignored.
//! Keys read by this version:
//! - `slots_per_key`: build slots allowed per (project, build key). Default
//!   [`DEFAULT_SLOTS_PER_KEY`]; the `DEV_HOST_SLOTS` variable and `--slots` override it.

use std::collections::BTreeMap;
use std::path::Path;

use crate::Vars;

pub const CONFIG_FILE: &str = "dev-host.conf";
pub const DEFAULT_SLOTS_PER_KEY: u32 = 8;
pub const SLOTS_VAR: &str = "DEV_HOST_SLOTS";

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
}
