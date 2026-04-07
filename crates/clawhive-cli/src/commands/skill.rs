use std::path::Path;

use anyhow::Result;
use clap::Subcommand;

use clawhive_core::SkillRegistry;

#[derive(Subcommand)]
pub(crate) enum SkillCommands {
    #[command(about = "List available skills")]
    List,
    #[command(about = "Show skill details")]
    Show {
        #[arg(help = "Skill name")]
        skill_name: String,
    },
    #[command(about = "Analyze a skill directory before install")]
    Analyze {
        #[arg(help = "Path to skill directory, or http(s) URL to SKILL.md")]
        source: String,
    },
    #[command(about = "Install a skill with permission/risk confirmation")]
    Install {
        #[arg(help = "Path to skill directory, or http(s) URL to SKILL.md")]
        source: String,
        #[arg(long, help = "Skip confirmation prompts")]
        yes: bool,
    },
    #[command(about = "Remove an installed skill")]
    Remove {
        #[arg(help = "Skill name")]
        skill_name: String,
    },
    #[command(about = "Update an installed skill from its original source")]
    Update {
        #[arg(help = "Skill name (omit for --all)")]
        skill_name: Option<String>,
        #[arg(long, help = "Update all skills with known sources")]
        all: bool,
    },
}

pub(crate) async fn run(cmd: SkillCommands, root: &Path) -> Result<()> {
    let skill_registry =
        SkillRegistry::load_from_dir(&root.join("skills")).unwrap_or_else(|_| SkillRegistry::new());
    match cmd {
        SkillCommands::List => {
            let skills = skill_registry.list();
            if skills.is_empty() {
                println!("No skills found in skills/ directory.");
            } else {
                println!(
                    "{:<20} {:<40} {:<10} SOURCE",
                    "NAME", "DESCRIPTION", "AVAILABLE"
                );
                println!("{}", "-".repeat(100));
                for skill in &skills {
                    let source = clawhive_core::skill_install::SkillMetadata::read_from(
                        &root.join("skills").join(&skill.name),
                    )
                    .and_then(|m| m.source)
                    .unwrap_or_else(|| "-".to_string());
                    let source_short = if source.len() > 28 {
                        format!("{}...", &source[..25])
                    } else {
                        source
                    };
                    println!(
                        "{:<20} {:<40} {:<10} {}",
                        skill.name,
                        if skill.description.len() > 38 {
                            format!("{}...", &skill.description[..35])
                        } else {
                            skill.description.clone()
                        },
                        if skill.requirements_met() {
                            "yes"
                        } else {
                            "no"
                        },
                        source_short,
                    );
                }
            }
        }
        SkillCommands::Show { skill_name } => match skill_registry.get(&skill_name) {
            Some(skill) => {
                println!("Skill: {}", skill.name);
                println!("Description: {}", skill.description);
                println!(
                    "Available: {}",
                    if skill.requirements_met() {
                        "yes"
                    } else {
                        "no"
                    }
                );
                if !skill.requires.bins.is_empty() {
                    println!("Required bins: {}", skill.requires.bins.join(", "));
                }
                if !skill.requires.env.is_empty() {
                    println!("Required env: {}", skill.requires.env.join(", "));
                }
                println!("\n--- Content ---\n{}", skill.content);

                if let Some(meta) = clawhive_core::skill_install::SkillMetadata::read_from(
                    &root.join("skills").join(&skill_name),
                ) {
                    println!("\n--- Install Info ---");
                    if let Some(ref source) = meta.source {
                        println!("Source: {source}");
                    }
                    if let Some(ref url) = meta.resolved_url {
                        println!("Resolved URL: {url}");
                    }
                    println!("Installed: {}", meta.installed_at);
                    if meta.high_risk_acknowledged {
                        println!("High risk: acknowledged");
                    }
                    if !meta.env_vars_written.is_empty() {
                        println!("Env vars: {}", meta.env_vars_written.join(", "));
                    }
                }
            }
            None => {
                anyhow::bail!("skill not found: {skill_name}");
            }
        },
        SkillCommands::Analyze { source } => {
            let resolved = clawhive_core::skill_install::resolve_skill_source(&source).await?;
            let report = clawhive_core::skill_install::analyze_skill_source(resolved.local_path())?;
            println!(
                "{}",
                clawhive_core::skill_install::render_skill_analysis(&report)
            );
        }
        SkillCommands::Remove { skill_name } => {
            let result = clawhive_core::skill_install::remove_skill(
                root,
                &root.join("skills"),
                &skill_name,
            )?;
            println!("Removed skill '{}'.", result.skill_name);
            if !result.env_vars_hint.is_empty() {
                println!(
                    "\nNote: the following env vars were set during install and may no longer be needed: {}\nYou can remove them from ~/.clawhive/.env if unused.",
                    result.env_vars_hint.join(", ")
                );
            }
        }
        SkillCommands::Update { skill_name, all } => {
            if all {
                let (updated, up_to_date, failed) =
                    clawhive_core::skill_install::update_all_skills(root, &root.join("skills"))
                        .await;
                if !updated.is_empty() {
                    println!("Updated: {}", updated.join(", "));
                }
                if !up_to_date.is_empty() {
                    println!("Already up to date: {}", up_to_date.join(", "));
                }
                for (name, err) in &failed {
                    println!("Failed to update {name}: {err}");
                }
                if updated.is_empty() && failed.is_empty() {
                    println!("All skills are up to date.");
                }
            } else if let Some(name) = skill_name {
                match clawhive_core::skill_install::update_skill(root, &root.join("skills"), &name)
                    .await?
                {
                    clawhive_core::skill_install::UpdateResult::Updated { .. } => {
                        println!("Updated skill '{name}'.");
                    }
                    clawhive_core::skill_install::UpdateResult::AlreadyUpToDate { .. } => {
                        println!("Skill '{name}' is already up to date.");
                    }
                }
            } else {
                anyhow::bail!("specify a skill name or use --all");
            }
        }
        SkillCommands::Install { source, yes } => {
            let resolved = clawhive_core::skill_install::resolve_skill_source(&source).await?;
            let report = clawhive_core::skill_install::analyze_skill_source(resolved.local_path())?;
            println!(
                "{}",
                clawhive_core::skill_install::render_skill_analysis(&report)
            );

            let high_risk = clawhive_core::skill_install::has_high_risk_findings(&report);
            let mut proceed = yes;
            if !yes {
                proceed = dialoguer::Confirm::new()
                    .with_prompt("Install this skill with the above permissions/risk profile?")
                    .default(false)
                    .interact()?;
                if !proceed {
                    println!("Installation cancelled.");
                }

                if proceed
                    && high_risk
                    && !dialoguer::Confirm::new()
                        .with_prompt("High-risk patterns detected. Confirm install anyway?")
                        .default(false)
                        .interact()?
                {
                    println!("Installation cancelled due to risk findings.");
                    proceed = false;
                }
            }

            if proceed {
                let installed = clawhive_core::skill_install::install_skill_from_analysis(
                    root,
                    &root.join("skills"),
                    resolved.local_path(),
                    &report,
                    yes || high_risk,
                    Some(&source),
                    resolved.resolved_url(),
                )?;
                println!(
                    "Installed skill '{}' to {}",
                    report.skill_name,
                    installed.target.display()
                );

                let env_vars = report.all_required_env_vars();
                let missing = clawhive_core::dotenv::missing_env_vars(&env_vars);
                if !missing.is_empty() {
                    let dotenv_path = clawhive_core::dotenv::dotenv_path_for_root(root);
                    println!(
                        "\nThis skill requires {} environment variable(s): {}",
                        missing.len(),
                        missing.join(", ")
                    );
                    for var in &missing {
                        let value: String = dialoguer::Input::new()
                            .with_prompt(format!("Enter value for {var}"))
                            .interact_text()?;
                        clawhive_core::dotenv::append_dotenv(&dotenv_path, var, &value)?;
                        println!("  Saved {var} to {}", dotenv_path.display());
                    }
                    clawhive_core::skill_install::update_env_vars_written(
                        &installed.target,
                        &missing,
                    )?;
                }
            }
        }
    }
    Ok(())
}
