# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 0.1.x   | :white_check_mark: |

## Reporting a Vulnerability

**Please do not report security vulnerabilities through public GitHub issues.**

Instead, please report them via email to: **security@nanocrab.dev** (or create a private security advisory on GitHub).

### What to Include

- Type of issue (e.g., buffer overflow, SQL injection, privilege escalation)
- Full paths of source file(s) related to the issue
- Location of the affected source code (tag/branch/commit or direct URL)
- Step-by-step instructions to reproduce the issue
- Proof-of-concept or exploit code (if possible)
- Impact of the issue, including how an attacker might exploit it

### Response Timeline

- **Initial response**: Within 48 hours
- **Status update**: Within 7 days
- **Fix timeline**: Depends on severity, typically within 30 days for critical issues

### Process

1. You report a vulnerability
2. We acknowledge receipt and begin investigation
3. We work on a fix and coordinate disclosure timeline with you
4. We release the fix and publish a security advisory
5. We credit you in the advisory (unless you prefer to remain anonymous)

## Security Considerations

### Current Security Controls

- **Tool allowlist**: Agents can only use explicitly allowed tools
- **Rate limiting**: Per-user token-bucket rate limiting at the gateway
- **Sub-agent bounds**: Depth limits and timeouts for sub-agent spawning
- **Repeat guard**: Prevents unbounded reasoning loops

### Known Limitations

- **No OS-level sandbox**: The WASM executor is a placeholder; tool execution currently runs in the native environment
- **Local file access**: File tools can read/write within configured paths
- **Shell execution**: Shell tool executes commands with the process's permissions

### Best Practices for Deployment

1. Run nanocrab with minimal privileges
2. Use a dedicated user account
3. Configure restrictive tool allowlists
4. Set appropriate rate limits
5. Monitor logs for suspicious activity
6. Keep the software updated

## Scope

This security policy applies to:
- The nanocrab binary and all official crates
- Official Docker images (when available)
- The install.sh script

It does not apply to:
- Third-party skills or plugins
- User-provided configurations
- Forks or unofficial distributions
