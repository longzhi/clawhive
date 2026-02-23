---
name: web_fetch
description: "Fetch and extract content from web URLs. Use when you need to: (1) Read a web page's content, (2) Download text from a URL, (3) Extract article text from a news site, (4) Get raw HTML or plain text from any URL. Uses curl via execute_command. For JavaScript-heavy sites or anti-bot protected pages, use the actionbook skill instead."
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

# Web Fetch - URL Content Retrieval

Fetch web page content using `curl` via the `execute_command` tool. This skill handles static pages and APIs. For JavaScript-rendered or anti-bot protected sites, use the `actionbook` skill instead.

## When to Use

- Fetching article text, documentation, blog posts
- Downloading API responses (JSON, XML)
- Reading plain HTML pages
- Getting raw content from URLs the user provides

## When NOT to Use (Use actionbook instead)

- Twitter/X content (requires JS rendering + anti-bot)
- Single-page apps (React, Vue, Angular)
- Sites requiring login/cookies
- Pages with heavy JavaScript rendering

## How to Fetch

Use `execute_command` to run `curl`. Always include these flags for reliability:

```bash
# Basic fetch (returns HTML)
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "URL"

# Follow redirects, 30s timeout, realistic User-Agent
```

### Flag Reference

| Flag | Purpose |
|------|---------|
| `-s` | Silent mode (no progress bar) |
| `-L` | Follow redirects |
| `-m 30` | Timeout after 30 seconds |
| `-A '...'` | Set User-Agent to avoid bot blocking |
| `-o /dev/null -w '%{http_code}'` | Check HTTP status only |
| `-H 'Accept: application/json'` | Request JSON response |

## Workflow Patterns

### Pattern 1: Fetch and Read a Web Page

```bash
# Step 1: Fetch the page
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "https://example.com/article"
```

The output will be HTML. Extract the relevant text content from the HTML and present it to the user.

### Pattern 2: Fetch JSON API

```bash
# Fetch JSON data
curl -sL -m 30 -H 'Accept: application/json' "https://api.example.com/data"
```

### Pattern 3: Check if URL is accessible

```bash
# Check status code first
curl -sL -m 10 -o /dev/null -w '%{http_code}' "https://example.com"
```

### Pattern 4: Fetch with text extraction (using sed/awk)

```bash
# Fetch and strip HTML tags for rough text extraction
curl -sL -m 30 -A 'Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36' "https://example.com" | sed 's/<[^>]*>//g' | sed '/^$/d' | head -200
```

### Pattern 5: Download to a file

```bash
# Save content to a file for later processing
curl -sL -m 60 -o /tmp/page.html "https://example.com/page"
```

## Content Extraction Tips

After fetching HTML, you can extract text by:

1. **Reading the raw HTML** and identifying the main content area
2. **Using sed** to strip tags: `sed 's/<[^>]*>//g'`
3. **Using grep** to find specific patterns: `grep -oP '(?<=<title>).*(?=</title>)'`

## Error Handling

- If curl times out → increase `-m` value or report URL is unreachable
- If 403/429 → site may block bots, suggest using `actionbook` skill with stealth mode
- If SSL error → try with `-k` flag (insecure, warn user)
- If empty response → check URL validity, try with different User-Agent

## Limitations

- Cannot execute JavaScript (use actionbook for JS-heavy sites)
- Cannot handle CAPTCHAs or bot challenges
- Cannot maintain session state across requests (use actionbook with profiles)
- Large pages may be truncated by execute_command output limits
