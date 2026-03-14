use anyhow::Result;
use async_trait::async_trait;
use clawhive_provider::ToolDef;

use crate::tool::{ToolContext, ToolExecutor, ToolOutput};

pub struct SkillTool;

impl SkillTool {
    pub fn new() -> Self {
        Self
    }
}

impl Default for SkillTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ToolExecutor for SkillTool {
    fn definition(&self) -> ToolDef {
        ToolDef {
            name: "skill".into(),
            description: "Read a skill's instructions or reference files. Pass the skill name to get its SKILL.md content. Optionally pass a file path to read a specific reference file within the skill.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Name of the skill (from Available Skills list)"
                    },
                    "file": {
                        "type": "string",
                        "description": "Optional: path to a reference file within the skill (e.g. 'references/commands.md'). If omitted, returns the main SKILL.md content."
                    }
                },
                "required": ["name"]
            }),
        }
    }

    async fn execute(&self, input: serde_json::Value, _ctx: &ToolContext) -> Result<ToolOutput> {
        let name = input["name"].as_str().unwrap_or("").trim();
        let file = input["file"]
            .as_str()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty());

        if name.is_empty() {
            return Ok(ToolOutput {
                content: "Error: skill name is required.".into(),
                is_error: true,
            });
        }

        let Some(registry) = _ctx.skill_registry() else {
            return Ok(ToolOutput {
                content: "Error: active skill registry is unavailable.".into(),
                is_error: true,
            });
        };

        match registry.get(name) {
            Some(skill) => {
                if let Some(file_path) = file {
                    match skill.read_reference_file(file_path) {
                        Ok(content) => Ok(ToolOutput {
                            content,
                            is_error: false,
                        }),
                        Err(e) => {
                            let available = skill.list_reference_files();
                            let list = if available.is_empty() {
                                "This skill has no reference files.".to_string()
                            } else {
                                format!("Available files: {}", available.join(", "))
                            };
                            Ok(ToolOutput {
                                content: format!("Error reading '{file_path}': {e}\n{list}"),
                                is_error: true,
                            })
                        }
                    }
                } else {
                    let refs = skill.list_reference_files();
                    let content = if refs.is_empty() {
                        skill.content.clone()
                    } else {
                        format!(
                            "{}\n\n---\n**Reference files available** (use `skill` tool with `file` parameter to read):\n{}",
                            skill.content,
                            refs.iter()
                                .map(|f| format!("- `{f}`"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        )
                    };
                    Ok(ToolOutput {
                        content,
                        is_error: false,
                    })
                }
            }
            None => {
                let available: Vec<_> = registry
                    .available()
                    .iter()
                    .map(|s| s.name.clone())
                    .collect();
                let list = if available.is_empty() {
                    "No skills are currently available.".to_string()
                } else {
                    format!("Available skills: {}", available.join(", "))
                };
                Ok(ToolOutput {
                    content: format!("Skill '{name}' not found. {list}"),
                    is_error: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use arc_swap::ArcSwap;

    use super::*;
    use crate::skill::SkillRegistry;
    use crate::tool::{ToolContext, ToolExecutor};
    use std::fs;
    use std::sync::Arc;

    fn create_test_skills_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let weather_dir = dir.path().join("weather");
        fs::create_dir_all(&weather_dir).unwrap();
        fs::write(
            weather_dir.join("SKILL.md"),
            "---\nname: weather\ndescription: Get weather forecasts\n---\n\n# Weather Skill\n\nUse `curl wttr.in` to get weather.",
        )
        .unwrap();
        dir
    }

    fn context_with_registry(skills_dir: &std::path::Path) -> ToolContext {
        let registry = SkillRegistry::load_from_dir(skills_dir).unwrap();
        ToolContext::builtin().with_skill_registry(Arc::new(registry))
    }

    #[tokio::test]
    async fn execute_returns_skill_content() {
        let dir = create_test_skills_dir();
        let tool = SkillTool::new();
        let ctx = context_with_registry(dir.path());
        let input = serde_json::json!({"name": "weather"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("# Weather Skill"));
        assert!(output.content.contains("curl wttr.in"));
    }

    #[tokio::test]
    async fn execute_returns_error_for_unknown_skill() {
        let dir = create_test_skills_dir();
        let tool = SkillTool::new();
        let ctx = context_with_registry(dir.path());
        let input = serde_json::json!({"name": "nonexistent"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("not found"));
        assert!(output.content.contains("weather"));
    }

    #[tokio::test]
    async fn definition_has_correct_schema() {
        let tool = SkillTool::new();
        let def = tool.definition();
        assert_eq!(def.name, "skill");
        assert!(def.description.contains("skill"));
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "name"));
    }

    #[tokio::test]
    async fn execute_with_file_returns_reference_content() {
        let dir = create_test_skills_dir();
        let refs_dir = dir.path().join("weather/references");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join("api.md"), "# Weather API\nEndpoint docs here").unwrap();

        let tool = SkillTool::new();
        let ctx = context_with_registry(dir.path());
        let input = serde_json::json!({"name": "weather", "file": "references/api.md"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("# Weather API"));
    }

    #[tokio::test]
    async fn execute_with_missing_file_lists_available() {
        let dir = create_test_skills_dir();
        let refs_dir = dir.path().join("weather/references");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join("api.md"), "docs").unwrap();

        let tool = SkillTool::new();
        let ctx = context_with_registry(dir.path());
        let input = serde_json::json!({"name": "weather", "file": "nonexistent.md"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(output.is_error);
        assert!(output.content.contains("references/api.md"));
    }

    #[tokio::test]
    async fn execute_without_file_mentions_available_references() {
        let dir = create_test_skills_dir();
        let refs_dir = dir.path().join("weather/references");
        fs::create_dir_all(&refs_dir).unwrap();
        fs::write(refs_dir.join("api.md"), "docs").unwrap();

        let tool = SkillTool::new();
        let ctx = context_with_registry(dir.path());
        let input = serde_json::json!({"name": "weather"});
        let output = tool.execute(input, &ctx).await.unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Reference files available"));
        assert!(output.content.contains("references/api.md"));
    }

    #[tokio::test]
    async fn execute_uses_cached_registry_until_reload() {
        let dir = create_test_skills_dir();
        let registry = SkillRegistry::load_from_dir(dir.path()).unwrap();
        let cache = ArcSwap::from_pointee(registry);
        let tool = SkillTool::new();
        let ctx = ToolContext::builtin().with_skill_registry(cache.load_full());

        let new_skill_dir = dir.path().join("calendar");
        fs::create_dir_all(&new_skill_dir).unwrap();
        fs::write(
            new_skill_dir.join("SKILL.md"),
            "---\nname: calendar\ndescription: Calendar skill\n---\n# Calendar",
        )
        .unwrap();

        let output = tool
            .execute(serde_json::json!({"name": "calendar"}), &ctx)
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output.content.contains("calendar"));
        assert!(output.content.contains("weather"));
    }
}
