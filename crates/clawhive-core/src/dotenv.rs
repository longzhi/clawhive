use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

fn default_dotenv_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|home| PathBuf::from(home).join(".clawhive").join(".env"))
}

fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        if !key.is_empty() {
            map.insert(key.to_string(), value.to_string());
        }
    }
    map
}

pub fn read_dotenv(path: &Path) -> HashMap<String, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => parse_dotenv(&content),
        Err(_) => HashMap::new(),
    }
}

pub fn resolve_env(key: &str) -> Option<String> {
    if let Ok(val) = std::env::var(key) {
        return Some(val);
    }
    let path = default_dotenv_path()?;
    let vars = read_dotenv(&path);
    vars.get(key).cloned()
}

pub fn append_dotenv(path: &Path, key: &str, value: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{key}={value}")
}

pub fn dotenv_path_for_root(config_root: &Path) -> PathBuf {
    config_root.join(".env")
}

pub fn missing_env_vars(required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|key| resolve_env(key).is_none())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_pairs() {
        let content = "FOO=bar\nBAZ=qux\n";
        let vars = parse_dotenv(content);
        assert_eq!(vars.get("FOO").unwrap(), "bar");
        assert_eq!(vars.get("BAZ").unwrap(), "qux");
    }

    #[test]
    fn parse_skips_comments_and_blanks() {
        let content = "# comment\n\nFOO=bar\n  # another comment\n";
        let vars = parse_dotenv(content);
        assert_eq!(vars.len(), 1);
        assert_eq!(vars.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn parse_strips_quotes() {
        let content = "A=\"hello world\"\nB='single quoted'\nC=unquoted\n";
        let vars = parse_dotenv(content);
        assert_eq!(vars.get("A").unwrap(), "hello world");
        assert_eq!(vars.get("B").unwrap(), "single quoted");
        assert_eq!(vars.get("C").unwrap(), "unquoted");
    }

    #[test]
    fn parse_handles_equals_in_value() {
        let content = "KEY=abc=def=ghi\n";
        let vars = parse_dotenv(content);
        assert_eq!(vars.get("KEY").unwrap(), "abc=def=ghi");
    }

    #[test]
    fn parse_trims_whitespace() {
        let content = "  FOO = bar  \n";
        let vars = parse_dotenv(content);
        assert_eq!(vars.get("FOO").unwrap(), "bar");
    }

    #[test]
    fn append_creates_file_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".env");
        append_dotenv(&path, "A", "1").unwrap();
        append_dotenv(&path, "B", "2").unwrap();
        let vars = read_dotenv(&path);
        assert_eq!(vars.get("A").unwrap(), "1");
        assert_eq!(vars.get("B").unwrap(), "2");
    }

    #[test]
    fn resolve_env_prefers_process_env() {
        let key = "CLAWHIVE_TEST_DOTENV_RESOLVE";
        std::env::set_var(key, "from_process");
        let val = resolve_env(key);
        assert_eq!(val, Some("from_process".to_string()));
        std::env::remove_var(key);
    }

    #[test]
    fn missing_env_vars_returns_absent_keys() {
        let missing = missing_env_vars(&[
            "CLAWHIVE_TEST_DEFINITELY_MISSING_1".into(),
            "CLAWHIVE_TEST_DEFINITELY_MISSING_2".into(),
        ]);
        assert_eq!(missing.len(), 2);
    }
}
