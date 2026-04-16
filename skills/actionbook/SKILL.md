---
name: actionbook
description: "MUST USE AUTOMATICALLY when user sends URLs from x.com, twitter.com, or any site that web-fetch fails on. Do NOT ask the user — just load this skill and use it. Browser automation and web scraping with anti-detection stealth mode. Capabilities: (1) Scrape Twitter/X content without login, (2) Extract data from anti-bot-protected websites, (3) Automate browser interactions (clicking, typing, screenshots), (4) Bypass JavaScript-rendered content that web-fetch cannot handle, (5) Access pages requiring cookies or sessions. Built on Chrome DevTools Protocol with fingerprint spoofing."
requires:
  bins:
    - actionbook
permissions:
  network:
    allow: ["*:443", "*:80"]
  exec: [actionbook, sh]
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
---

# Actionbook - Stealth Browser Automation

Browser automation via Chrome DevTools Protocol (CDP) using your existing Chrome/Brave/Edge. Anti-detection built-in.

## Auto-Trigger Rules

Use this skill automatically — do NOT ask the user — when:

1. **URL contains `x.com` or `twitter.com`** → Use with `--stealth` flag
2. **`web-fetch` returned an error or empty content** → Retry with actionbook
3. **User asks to scrape, screenshot, or interact with a website** → Use directly
4. **URL points to a JavaScript-heavy SPA** → Prefer over web-fetch

**Standard workflow:**

1. `actionbook --stealth browser open "<URL>"`
2. `actionbook browser wait '<selector>'` if needed for dynamic content
3. Extract with `actionbook browser eval` or `actionbook browser snapshot`
4. `actionbook browser close` when done

## Quick Reference

```bash
# Browser control
actionbook browser open <URL>        # Open URL in new browser
actionbook browser goto <URL>        # Navigate current page
actionbook browser close             # Close browser

# Content extraction
actionbook browser eval <JS>         # Execute JavaScript
actionbook browser snapshot          # Get accessibility tree
actionbook browser screenshot [PATH] # Take screenshot

# Interaction
actionbook browser click <SELECTOR>          # Click element
actionbook browser type <SELECTOR> <TEXT>    # Type into input
actionbook browser wait <SELECTOR>           # Wait for element

# Cookies & state
actionbook browser cookies list              # List all cookies
actionbook browser cookies get <NAME>        # Get specific cookie
actionbook browser cookies set <NAME> <VAL>  # Set cookie
```

### Global Flags

```bash
--stealth                    # Enable anti-detection (required for Twitter/X)
--stealth-os <OS>            # Spoof OS (macos-arm, windows, linux)
--stealth-gpu <GPU>          # Spoof GPU (apple-m4-max, rtx4080, etc.)
--profile <NAME>             # Use isolated browser session
--headless                   # Run browser invisibly
```

## Workflow Patterns

### Pattern 1: Twitter/X Content Extraction

```bash
# Step 1: Open with stealth
actionbook --stealth browser open "https://x.com/elonmusk"

# Step 2: Wait for timeline
actionbook browser wait '[data-testid="primaryColumn"]'

# Step 3: Extract tweets
actionbook browser eval '
  Array.from(document.querySelectorAll("[data-testid=\"tweetText\"]"))
    .map(el => el.innerText)
    .join("\n\n---\n\n")
'

# Step 4: Close
actionbook browser close
```

For structured extraction with authors:

```bash
actionbook browser eval '
  Array.from(document.querySelectorAll("[data-testid=\"tweetText\"]")).map(t => ({
    text: t.innerText,
    author: t.closest("article")?.querySelector("[data-testid=\"User-Name\"]")?.innerText
  }))
'
```

If "Log in to see more" appears, create a persistent profile (see Pattern 3).

### Pattern 2: Form Submission

```bash
actionbook browser type '#email' "user@example.com"
actionbook browser type '#password' "secret"
actionbook browser click 'button[type="submit"]'
actionbook browser wait '.dashboard'
actionbook browser eval 'document.querySelector(".welcome-message").innerText'
```

### Pattern 3: Persistent Session (Login Once, Reuse)

```bash
# First time: create profile and login manually
actionbook profile create my-service
actionbook --profile my-service browser open "https://service.com/login"

# Future sessions: cookies preserved
actionbook --profile my-service browser goto "https://service.com/dashboard"
```

### Pattern 4: Stealth with Custom Fingerprint

```bash
actionbook --stealth --stealth-os windows --stealth-gpu rtx4080 browser open "https://bot-check.com"
```

Stealth applies: navigator overrides, WebGL fingerprint spoofing, plugin injection, automation flag removal.

**OS profiles:** `macos-arm`, `macos-intel`, `windows`, `linux`
**GPU profiles:** `apple-m4-max`, `rtx4080`, `gtx1660`, `intel-uhd630`

## Troubleshooting

| Problem | Fix |
|---------|-----|
| "Element not found" | Add `actionbook browser wait '<selector>'` before extraction |
| "Connection refused" / "CDP not ready" | Run `actionbook browser close` then re-open |
| Twitter "Log in to see more" | Ensure `--stealth` flag is set, or use a logged-in `--profile` |

## Resources

- **`scripts/fetch_tweet.sh`** — Simplified Twitter extraction wrapper
- **`references/commands.md`** — Complete command reference
- **`references/selectors.md`** — CSS selectors for popular sites
