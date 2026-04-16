---
name: web-fetch
description: "Fetch and extract content from web URLs using curl. Use when the user provides a URL, asks to read a web page, download article text, check if a site is reachable, or retrieve API responses (JSON, XML, HTML). Runs curl via execute_command with redirect-following, timeouts, and realistic User-Agent headers. Handles static pages, REST APIs, and RSS feeds. For JavaScript-rendered SPAs or anti-bot-protected sites (Twitter/X, login-gated pages), use the actionbook skill instead."
requires:
  bins:
    - curl
  env: []
permissions:
  network:
    allow: ["*:443", "*:80"]
  exec: [curl, sh, sed, awk, grep, head]
  fs:
    read: ["$SKILL_DIR/**"]
    write: ["$WORK_DIR/**"]
---

# Web Fetch

Fetch web page content using `curl` via `execute_command`. For JavaScript-rendered or anti-bot protected sites, use the `actionbook` skill instead.

## When NOT to Use (Use actionbook)

- Twitter/X URLs — always blocked without JS rendering
- SPAs (React, Vue, Angular dashboards)
- Sites requiring login/cookies
- Pages returning empty/useless content from curl

## Standard Fetch Command

```bash
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "<URL>"
```

Flags: `-s` silent, `-L` follow redirects, `-m 30` timeout, `-A` realistic User-Agent.

## Workflow Patterns

### 1. Fetch and Read a Web Page

```bash
# Fetch HTML, then extract relevant text content for the user
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "https://example.com/article"
```

### 2. Fetch JSON API

```bash
curl -sL -m 30 -H 'Accept: application/json' "https://api.example.com/data"
```

### 3. Check URL Accessibility

```bash
curl -sL -m 10 -o /dev/null -w '%{http_code}' "https://example.com"
```

### 4. Fetch with Text Extraction

```bash
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "https://example.com" | sed 's/<[^>]*>//g' | sed '/^$/d' | head -200
```

### 5. Download to File

```bash
curl -sL -m 60 -o /tmp/page.html "https://example.com/page"
```

## Error Handling

| Error | Action |
|-------|--------|
| Timeout | Increase `-m` value or report URL unreachable |
| 403/429 | Site blocks bots → switch to `actionbook` with `--stealth` |
| SSL error | Retry with `-k` (warn user about insecure connection) |
| Empty response | Verify URL, try different User-Agent, or switch to `actionbook` |
