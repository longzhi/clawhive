---
name: example
description: "Template and reference for creating new clawhive skills. Use when the user asks to create a new skill, scaffold a SKILL.md file, understand the skill format, or add a custom capability to a clawhive agent. Demonstrates YAML frontmatter structure, permission declarations, required binaries/env, and content organization patterns."
requires:
  bins: []
  env: []
---

# Example Skill — Skill Authoring Reference

Reference template for creating new clawhive skills. Skills are loaded from `skills/<name>/SKILL.md` and injected into agent system prompts.

## When to Use

- User asks to create a new skill or add a capability to an agent
- User wants to understand the SKILL.md format
- Scaffolding a new skill directory

## Skill Structure

```
skills/<skill-name>/
├── SKILL.md              # Required — frontmatter + instructions
├── references/           # Optional — detailed docs, API refs
│   └── commands.md
└── scripts/              # Optional — helper scripts
    └── helper.sh
```

## SKILL.md Format

### 1. YAML Frontmatter (Required)

```yaml
---
name: my-skill              # kebab-case, matches folder name
description: "Short description of what this skill does. Use when the user asks to <trigger condition>. Include specific actions and keywords for routing."
requires:
  bins:                     # Binaries that must be in PATH
    - my-tool
  env:                      # Required environment variables
    - MY_API_KEY
permissions:
  network:
    allow: ["*:443"]        # Allowed network destinations
  exec: [my-tool, sh]      # Allowed executables
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
---
```

### 2. Body Content (Required)

Structure the body with:

1. **One-line summary** — What this skill does
2. **When to Use** — Trigger conditions with concrete keywords
3. **Workflow** — Numbered steps with executable commands
4. **Error handling** — Common failures and recovery actions

### 3. Creating a New Skill

```bash
# 1. Create the skill directory (kebab-case name)
mkdir -p skills/my-new-skill

# 2. Create SKILL.md with frontmatter and body
# 3. Test with: clawhive chat --agent <agent-with-skill>
# 4. Validate config: clawhive validate
```

## Best Practices

- **Name**: kebab-case only (`my-skill`, not `my_skill` or `MySkill`)
- **Description**: Include "Use when..." clause with natural trigger keywords
- **Permissions**: Declare the minimum required — exec, fs, network
- **Body**: Provide executable commands, not abstract advice
- **Progressive disclosure**: Keep SKILL.md concise, put detailed references in `references/`
