# Skill Permission Scope Fix

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Skill permissions should only constrain exec/fs/network when a skill is explicitly invoked via `/skill <name>`, not globally affect all requests when any skill is installed.

**Architecture:** Change `merged_permissions` computation in normal mode (non-forced-skill) from merging all active skills' permissions to returning `None`. This makes normal-mode requests use `ToolOrigin::Builtin` (only HardBaseline + ExecSecurityConfig protect), while `/skill <name>` requests continue using `ToolOrigin::External` (skill permissions enforced). Two call sites in `orchestrator.rs` (`handle_inbound` and `handle_inbound_stream`) share identical logic and both need the same one-line change.

**Tech Stack:** Rust, clawhive-core crate, corral-core (Permissions)

**Backward Compatibility:** Normal-mode behavior reverts to pre-skill-installation state (only HardBaseline + agent-level ExecSecurityConfig). Forced skill mode (`/skill <name>`) behavior unchanged.

---

### Task 1: Update existing test to reflect correct behavior

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs:2277-2317` (inline test)

**Step 1: Rewrite the test**

The existing test `merged_permissions_in_normal_mode_use_all_active_skills` asserts the **buggy** behavior (normal mode merges all skill permissions). Rename and rewrite it to assert the **correct** behavior: `compute_merged_permissions` with `forced_skills=None` still merges all skills (the function itself is unchanged), but a new test verifies the orchestrator's intent that normal mode should not use skill permissions.

Replace the test with:

```rust
#[test]
fn compute_merged_permissions_merges_all_when_no_forced() {
    let dir = tempfile::tempdir().unwrap();

    let skill_a = dir.path().join("skill-a");
    std::fs::create_dir_all(&skill_a).unwrap();
    std::fs::write(
        skill_a.join("SKILL.md"),
        r#"---
name: skill-a
description: A
permissions:
  network:
    allow: ["api.a.com:443"]
---
Body"#,
    )
    .unwrap();

    let skill_b = dir.path().join("skill-b");
    std::fs::create_dir_all(&skill_b).unwrap();
    std::fs::write(
        skill_b.join("SKILL.md"),
        r#"---
name: skill-b
description: B
permissions:
  network:
    allow: ["api.b.com:443"]
---
Body"#,
    )
    .unwrap();

    let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

    // compute_merged_permissions with no forced skills still returns the union
    // (the function is a utility; the CALLER decides whether to use it)
    let merged = Orchestrator::compute_merged_permissions(&active_skills, None);
    let perms = merged.expect("compute_merged_permissions returns Some when skills have perms");
    assert!(perms.network.allow.contains(&"api.a.com:443".to_string()));
    assert!(perms.network.allow.contains(&"api.b.com:443".to_string()));
}
```

**Step 2: Run test to verify it passes**

Run: `cargo test -p clawhive-core -- orchestrator::tests::compute_merged_permissions_merges_all_when_no_forced -v`
Expected: PASS (we only renamed, logic is unchanged)

**Step 3: Commit**

```bash
git add crates/clawhive-core/src/orchestrator.rs
git commit -m "test: rename merged_permissions test to clarify compute utility behavior"
```

---

### Task 2: Add tests for normal-mode skill permission scoping

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs` (add tests in `mod tests`)

**Step 1: Write failing tests**

Add these tests at the end of `mod tests`:

```rust
#[test]
fn normal_mode_should_not_use_skill_permissions() {
    // In normal mode (no forced skill), the orchestrator should pass
    // merged_permissions=None to tool_use_loop, resulting in Builtin origin.
    // This test verifies the design intent: installing skills with permissions
    // should NOT restrict normal (non-skill) requests.

    let dir = tempfile::tempdir().unwrap();

    let skill = dir.path().join("restricted-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        r#"---
name: restricted-skill
description: Only allows sh
permissions:
  exec: [sh]
  fs:
    read: ["$SKILL_DIR/**"]
---
Body"#,
    )
    .unwrap();

    let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

    // Verify the skill has permissions declared
    let skill_entry = active_skills.get("restricted-skill").unwrap();
    assert!(skill_entry.permissions.is_some());

    // In normal mode, orchestrator should NOT apply these permissions.
    // The forced_skill_names() returns None for normal messages.
    let forced_skills: Option<Vec<String>> = None;

    // Normal mode: merged_permissions should be None (Builtin origin)
    let merged_permissions = if forced_skills.is_some() {
        Orchestrator::compute_merged_permissions(&active_skills, forced_skills.as_deref())
    } else {
        None // ← This is the fix we're implementing
    };

    assert!(
        merged_permissions.is_none(),
        "normal mode must not use skill permissions"
    );
}

#[test]
fn forced_skill_mode_applies_skill_permissions() {
    let dir = tempfile::tempdir().unwrap();

    let skill = dir.path().join("restricted-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        r#"---
name: restricted-skill
description: Only allows sh
permissions:
  exec: [sh]
  network:
    allow: ["api.example.com:443"]
---
Body"#,
    )
    .unwrap();

    let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

    // Forced skill mode: permissions SHOULD be applied
    let forced = Some(vec!["restricted-skill".to_string()]);
    let merged = Orchestrator::compute_merged_permissions(
        &active_skills,
        forced.as_deref(),
    );

    let perms = merged.expect("forced skill mode must return permissions");
    assert_eq!(perms.exec, vec!["sh".to_string()]);
    assert!(perms.network.allow.contains(&"api.example.com:443".to_string()));
}

#[test]
fn forced_skill_without_permissions_returns_none() {
    let dir = tempfile::tempdir().unwrap();

    let skill = dir.path().join("no-perms-skill");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        r#"---
name: no-perms-skill
description: No permissions declared
---
Body"#,
    )
    .unwrap();

    let active_skills = SkillRegistry::load_from_dir(dir.path()).unwrap();

    // Forced skill with no permissions → None (Builtin, no extra restrictions)
    let forced = Some(vec!["no-perms-skill".to_string()]);
    let merged = Orchestrator::compute_merged_permissions(
        &active_skills,
        forced.as_deref(),
    );

    assert!(
        merged.is_none(),
        "skill without permissions should not trigger External origin"
    );
}
```

**Step 2: Run tests to verify they pass**

Run: `cargo test -p clawhive-core -- orchestrator::tests::normal_mode_should_not_use_skill_permissions orchestrator::tests::forced_skill_mode_applies_skill_permissions orchestrator::tests::forced_skill_without_permissions_returns_none -v`
Expected: All PASS (these tests encode the correct behavior using the same logic as the fix)

**Step 3: Commit**

```bash
git add crates/clawhive-core/src/orchestrator.rs
git commit -m "test: add skill permission scoping tests for normal vs forced mode"
```

---

### Task 3: Fix handle_inbound — normal mode returns None

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs:941-942`

**Step 1: Apply the fix**

Change line 942 from:

```rust
        } else {
            Self::compute_merged_permissions(&active_skills, None)
        };
```

to:

```rust
        } else {
            // Normal mode: no skill permissions applied.
            // Agent-level ExecSecurityConfig + HardBaseline provide protection.
            // Skill permissions only activate during forced skill invocation (/skill <name>).
            None
        };
```

**Step 2: Run tests**

Run: `cargo test -p clawhive-core -- orchestrator -v`
Expected: All PASS

**Step 3: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS (no unused code warnings since `compute_merged_permissions` is still used by the forced-skill branch and by `handle_inbound_stream`)

**Step 4: Commit**

```bash
git add crates/clawhive-core/src/orchestrator.rs
git commit -m "fix: normal mode requests no longer constrained by installed skill permissions

Previously, installing any skill with declared permissions caused ALL
requests (including scheduled tasks and normal conversations) to use
External origin, restricting exec/fs/network to the skill's whitelist.

Now only explicit skill invocations (/skill <name>) activate skill-level
permission constraints. Normal requests use Builtin origin, protected by
HardBaseline + agent-level ExecSecurityConfig."
```

---

### Task 4: Fix handle_inbound_stream — same change

**Files:**
- Modify: `crates/clawhive-core/src/orchestrator.rs:1178-1179`

**Step 1: Apply the fix**

Change line 1179 from:

```rust
        } else {
            Self::compute_merged_permissions(&active_skills, None)
        };
```

to:

```rust
        } else {
            // Normal mode: no skill permissions applied (same as handle_inbound).
            None
        };
```

**Step 2: Run full quality gate**

Run: `just check`
Expected: fmt ✅, clippy ✅, all tests ✅

**Step 3: Commit**

```bash
git add crates/clawhive-core/src/orchestrator.rs
git commit -m "fix: apply same normal-mode permission fix to streaming handler"
```

---

### Task 5: Verify end-to-end

**Step 1: Run full test suite**

Run: `cargo test --workspace`
Expected: All tests pass

**Step 2: Run clippy**

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: No warnings

**Step 3: Verify specific test scenarios**

Run: `cargo test -p clawhive-core -- orchestrator::tests -v`
Expected output includes:
- `compute_merged_permissions_merges_all_when_no_forced ... ok`
- `normal_mode_should_not_use_skill_permissions ... ok`
- `forced_skill_mode_applies_skill_permissions ... ok`
- `forced_skill_without_permissions_returns_none ... ok`

**Step 4: Final commit (if any fixups needed)**

```bash
git add -A
git commit -m "chore: final verification pass"
```
