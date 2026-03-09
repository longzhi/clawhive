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
                println!("{:<20} {:<50} {:<10}", "NAME", "DESCRIPTION", "AVAILABLE");
                println!("{}", "-".repeat(80));
                for skill in &skills {
                    println!(
                        "{:<20} {:<50} {:<10}",
                        skill.name,
                        if skill.description.len() > 48 {
                            format!("{}...", &skill.description[..45])
                        } else {
                            skill.description.clone()
                        },
                        if skill.requirements_met() {
                            "yes"
                        } else {
                            "no"
                        },
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
                )?;
                println!(
                    "Installed skill '{}' to {}",
                    report.skill_name,
                    installed.target.display()
                );
            }
        }
    }
    Ok(())
}
