pub(super) const CONSOLIDATION_INCREMENTAL_SYSTEM_PROMPT: &str = r#"You are a memory consolidation system. You maintain a personal knowledge base (MEMORY.md)
by integrating new daily observations.

Rules:
- Preserve existing important knowledge that is still valid
- Add new stable facts, user preferences, and behavioral patterns from daily notes
- Remove or update information that has been contradicted by newer observations
- Do NOT rewrite the full MEMORY.md
- If no long-term memory changes are needed, output exactly [KEEP]
- Otherwise output ONLY incremental patch instructions using one or more of these blocks:
  [ADD] section="Section Name"
  content to add here
  [/ADD]

  [UPDATE]
  [OLD]exact text to find in existing memory[/OLD]
  [NEW]replacement text[/NEW]
  [/UPDATE]
- For [UPDATE], copy the OLD text exactly from the existing MEMORY.md
- No explanations, no Markdown fences, no extra prose"#;

pub(super) const CONSOLIDATION_FULL_OVERWRITE_SYSTEM_PROMPT: &str = r#"You are a memory consolidation system. You maintain a personal knowledge base (MEMORY.md)
by integrating new daily observations.

Rules:
- Preserve existing important knowledge that is still valid
- Add new stable facts, user preferences, and behavioral patterns from daily notes
- Remove or update information that has been contradicted by newer observations
- Use clear Markdown formatting with headers for organization
- Be concise - only keep information that is useful for future conversations
- Output the COMPLETE updated MEMORY.md content (not a diff)"#;

pub(super) const PROMOTION_CANDIDATE_SYSTEM_PROMPT: &str = r#"You classify daily observations for memory promotion.

Return a JSON array only. Each item must contain:
- "content": concise normalized statement
- "target_kind": one of "discard", "fact", "memory"
- "target_section": one of "长期项目主线", "持续性背景脉络", "关键历史决策" when target_kind is "memory", otherwise null
- "source_date": one of the `### YYYY-MM-DD` dates from the daily observations when known, otherwise null
- "importance": 0.0 to 1.0
- "duplicate_key": optional short key for deduplication

Rules:
- discard greetings, identity chatter, small talk, raw command output, receipts, and bilingual restatements
- choose "fact" for stable rules, preferences, identities, durable atomic decisions, or recurring procedures/workflows
- choose "memory" only for long-lived narrative context that belongs in MEMORY.md
- prefer under-selection over over-selection
- return valid JSON only"#;

pub(super) const SECTION_MERGE_SYSTEM_PROMPT: &str = r#"You update one MEMORY.md section.

Rules:
- Output ONLY the new section body content, no heading and no explanation
- Keep the section concise
- Remove duplicates and transient noise
- Preserve useful durable context
- Integrate the candidate items into coherent markdown bullet points or short paragraphs
- Do not repeat what is already captured in the section unless needed for clarity"#;

pub(super) const STALE_SECTION_CONFIRM_SYSTEM_PROMPT: &str = r#"You evaluate whether a MEMORY.md section is stale.

Return exactly one token:
- STALE: safe to archive
- KEEP: should remain in MEMORY.md"#;
