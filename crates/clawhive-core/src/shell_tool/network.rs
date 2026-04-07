/// Extract target hosts from command arguments (best-effort URL parsing)
pub(super) fn extract_network_targets(command: &str) -> Vec<(String, u16)> {
    let mut targets = Vec::new();
    for token in command.split_whitespace() {
        if let Ok(url) = reqwest::Url::parse(token) {
            if let Some(host) = url.host_str() {
                let port = url.port_or_known_default().unwrap_or(443);
                targets.push((host.to_string(), port));
            }
        }
    }
    targets
}

/// Known package manager commands and their registry domains
pub(super) fn package_manager_domains(command: &str) -> Vec<String> {
    let first_token = command.split_whitespace().next().unwrap_or("");
    let basename = std::path::Path::new(first_token)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first_token);
    match basename {
        "npm" | "npx" | "yarn" | "pnpm" => {
            vec!["registry.npmjs.org".into(), "registry.yarnpkg.com".into()]
        }
        "pip" | "pip3" => vec!["pypi.org".into(), "files.pythonhosted.org".into()],
        "cargo" => vec!["crates.io".into(), "static.crates.io".into()],
        "gem" => vec!["rubygems.org".into()],
        "go" => vec!["proxy.golang.org".into()],
        _ => vec![],
    }
}

/// Check if a network target matches a domain pattern from the whitelist
pub(super) fn domain_matches(pattern: &str, host: &str) -> bool {
    if pattern == host {
        return true;
    }
    // Wildcard: *.example.com matches sub.example.com
    if let Some(suffix) = pattern.strip_prefix("*.") {
        return host.ends_with(suffix) && host.len() > suffix.len();
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_network_targets_finds_urls() {
        let targets = extract_network_targets("git clone https://github.com/user/repo.git");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "github.com");
        assert_eq!(targets[0].1, 443);
    }

    #[test]
    fn extract_network_targets_finds_http_urls() {
        let targets = extract_network_targets("curl http://example.com:8080/api");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].0, "example.com");
        assert_eq!(targets[0].1, 8080);
    }

    #[test]
    fn extract_network_targets_no_urls() {
        let targets = extract_network_targets("ls -la /tmp");
        assert!(targets.is_empty());
    }

    #[test]
    fn package_manager_domains_npm() {
        let domains = package_manager_domains("npm install express");
        assert!(domains.iter().any(|d| d.contains("npmjs.org")));
    }

    #[test]
    fn package_manager_domains_pip() {
        let domains = package_manager_domains("pip install requests");
        assert!(domains.iter().any(|d| d.contains("pypi.org")));
    }

    #[test]
    fn package_manager_domains_unknown() {
        let domains = package_manager_domains("echo hello");
        assert!(domains.is_empty());
    }

    #[test]
    fn domain_matches_exact() {
        assert!(domain_matches("github.com", "github.com"));
        assert!(!domain_matches("github.com", "api.github.com"));
    }

    #[test]
    fn domain_matches_wildcard() {
        assert!(domain_matches("*.github.com", "api.github.com"));
        assert!(domain_matches(
            "*.github.com",
            "raw.githubusercontent.github.com"
        ));
        assert!(!domain_matches("*.github.com", "github.com"));
    }
}
