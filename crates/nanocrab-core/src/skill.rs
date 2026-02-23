use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillPermissions {
    #[serde(default)]
    pub fs: FsPermissionsDef,
    #[serde(default)]
    pub network: NetworkPermissionsDef,
    #[serde(default)]
    pub exec: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub services: HashMap<String, ServicePermissionDef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsPermissionsDef {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkPermissionsDef {
    #[serde(default)]
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServicePermissionDef {
    pub access: String,
    #[serde(default)]
    pub scope: HashMap<String, serde_json::Value>,
}

impl SkillPermissions {
    pub fn to_corral_permissions(&self) -> corral_core::Permissions {
        let mut services = std::collections::HashMap::new();
        for (name, def) in &self.services {
            services.insert(
                name.clone(),
                corral_core::ServicePermission {
                    access: def.access.clone(),
                    scope: def.scope.clone(),
                },
            );
        }
        corral_core::Permissions {
            fs: corral_core::FsPermissions {
                read: self.fs.read.clone(),
                write: self.fs.write.clone(),
            },
            network: corral_core::NetworkPermissions {
                allow: self.network.allow.clone(),
            },
            exec: self.exec.clone(),
            env: self.env.clone(),
            services,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct SkillFrontmatter {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub requires: SkillRequirements,
    #[serde(default)]
    pub permissions: Option<SkillPermissions>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct SkillRequirements {
    #[serde(default)]
    pub bins: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub requires: SkillRequirements,
    pub permissions: Option<SkillPermissions>,
    pub content: String,
    pub path: PathBuf,
}

impl Skill {
    pub fn requirements_met(&self) -> bool {
        for bin in &self.requires.bins {
            if !bin_exists(bin) {
                return false;
            }
        }
        for env_var in &self.requires.env {
            if std::env::var(env_var).is_err() {
                return false;
            }
        }
        true
    }
}

fn bin_exists(name: &str) -> bool {
    std::process::Command::new("which")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
pub struct SkillRegistry {
    skills: HashMap<String, Skill>,
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_dir(dir: &Path) -> Result<Self> {
        let mut registry = Self::new();
        if !dir.exists() {
            return Ok(registry);
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let skill_dir = entry.path();
            if !skill_dir.is_dir() {
                continue;
            }
            let skill_md = skill_dir.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            match load_skill(&skill_md) {
                Ok(skill) => {
                    registry.skills.insert(skill.name.clone(), skill);
                }
                Err(e) => {
                    tracing::warn!("Failed to load skill from {}: {e}", skill_md.display());
                }
            }
        }
        Ok(registry)
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }

    pub fn list(&self) -> Vec<&Skill> {
        let mut skills: Vec<_> = self.skills.values().collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    pub fn available(&self) -> Vec<&Skill> {
        let mut skills: Vec<_> = self
            .skills
            .values()
            .filter(|s| s.requirements_met())
            .collect();
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        skills
    }

    pub fn summary_prompt(&self) -> String {
        let available = self.available();
        if available.is_empty() {
            return String::new();
        }
        let mut lines = vec!["## Available Skills".to_string()];
        for skill in &available {
            lines.push(format!("- **{}**: {}", skill.name, skill.description));
        }
        lines.push(
            "\nTo use a skill, ask to read the full SKILL.md for detailed instructions."
                .to_string(),
        );
        lines.join("\n")
    }
}

fn load_skill(path: &Path) -> Result<Skill> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let (frontmatter, content) = parse_frontmatter(&raw)?;
    Ok(Skill {
        name: frontmatter.name,
        description: frontmatter.description,
        requires: frontmatter.requires,
        permissions: frontmatter.permissions,
        content,
        path: path.to_path_buf(),
    })
}

fn parse_frontmatter(raw: &str) -> Result<(SkillFrontmatter, String)> {
    let trimmed = raw.trim_start();
    if !trimmed.starts_with("---") {
        anyhow::bail!("SKILL.md must start with YAML frontmatter (---)");
    }
    let after_first = &trimmed[3..];
    let end = after_first
        .find("---")
        .ok_or_else(|| anyhow::anyhow!("no closing --- for frontmatter"))?;
    let yaml_str = &after_first[..end];
    let content = after_first[end + 3..].trim().to_string();
    let fm: SkillFrontmatter =
        serde_yaml::from_str(yaml_str).context("parsing skill frontmatter YAML")?;
    Ok((fm, content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_frontmatter_extracts_fields() {
        let raw = "---\nname: test-skill\ndescription: A test skill\nrequires:\n  bins: []\n  env: []\n---\n\n# Content\nHello world";
        let (fm, content) = parse_frontmatter(raw).unwrap();
        assert_eq!(fm.name, "test-skill");
        assert_eq!(fm.description, "A test skill");
        assert!(content.contains("# Content"));
    }

    #[test]
    fn parse_frontmatter_rejects_missing() {
        let raw = "# No frontmatter\nJust content";
        assert!(parse_frontmatter(raw).is_err());
    }

    #[test]
    fn parse_frontmatter_with_permissions() {
        let raw = r#"---
name: web-fetch
description: Fetch web content
requires:
  bins: [curl]
permissions:
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
  network:
    allow: ["*:443", "*:80"]
  exec: [curl, jq]
  env: [LANG]
---
Content here"#;
        let (fm, _content) = parse_frontmatter(raw).unwrap();
        let perms = fm.permissions.unwrap();
        assert_eq!(perms.fs.read.len(), 1);
        assert_eq!(perms.fs.write.len(), 1);
        assert_eq!(perms.network.allow.len(), 2);
        assert_eq!(perms.exec, vec!["curl", "jq"]);
        assert_eq!(perms.env, vec!["LANG"]);
    }

    #[test]
    fn parse_frontmatter_without_permissions_is_none() {
        let raw = "---\nname: simple\ndescription: No perms\n---\nBody";
        let (fm, _) = parse_frontmatter(raw).unwrap();
        assert!(fm.permissions.is_none());
    }

    #[test]
    fn skill_permissions_to_corral_basic() {
        let sp = SkillPermissions {
            fs: FsPermissionsDef {
                read: vec!["src/**".into()],
                write: vec![],
            },
            network: NetworkPermissionsDef {
                allow: vec!["api.com:443".into()],
            },
            exec: vec!["curl".into()],
            env: vec!["HOME".into()],
            services: Default::default(),
        };
        let corral_perms = sp.to_corral_permissions();
        assert_eq!(corral_perms.fs.read, vec!["src/**"]);
        assert_eq!(corral_perms.network.allow, vec!["api.com:443"]);
        assert_eq!(corral_perms.exec, vec!["curl"]);
        assert_eq!(corral_perms.env, vec!["HOME"]);
    }

    #[test]
    fn load_from_dir_loads_skills() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("my-skill");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: my-skill\ndescription: test\n---\nContent here",
        )
        .unwrap();

        let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.get("my-skill").unwrap().name, "my-skill");
    }

    #[test]
    fn summary_prompt_formats_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("demo");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nBody",
        )
        .unwrap();

        let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
        let prompt = registry.summary_prompt();
        assert!(prompt.contains("## Available Skills"));
        assert!(prompt.contains("**demo**"));
    }

    #[test]
    fn requirements_met_checks_env() {
        let skill = Skill {
            name: "test".into(),
            description: "test".into(),
            requires: SkillRequirements {
                bins: vec![],
                env: vec!["NANOCRAB_NONEXISTENT_VAR_12345".into()],
            },
            permissions: None,
            content: String::new(),
            path: PathBuf::new(),
        };
        assert!(!skill.requirements_met());
    }
}
