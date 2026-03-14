#!/bin/bash
# Simplified Twitter/X content fetcher
# Usage: ./fetch_tweet.sh <tweet_url> [output_file]

set -e

ACTIONBOOK="$(command -v actionbook)" || { echo "actionbook not found in PATH"; exit 1; }
TWEET_URL="$1"
OUTPUT="${2:-tweet_output.txt}"

if [ -z "$TWEET_URL" ]; then
  echo "Usage: $0 <tweet_url> [output_file]"
  echo "Example: $0 https://x.com/username/status/123456789 tweet.txt"
  exit 1
fi

echo "🔧 Opening Twitter with stealth mode..."
$ACTIONBOOK --stealth browser open "$TWEET_URL" > /dev/null 2>&1

echo "⏳ Waiting for page load..."
sleep 3

echo "📝 Extracting content..."
CONTENT=$($ACTIONBOOK browser eval 'document.body.innerText')

echo "💾 Saving to $OUTPUT..."
echo "$CONTENT" > "$OUTPUT"

echo "📸 Taking screenshot..."
$ACTIONBOOK browser screenshot "${OUTPUT%.txt}.png"

echo "🧹 Closing browser..."
$ACTIONBOOK browser close

echo "✅ Done! Content saved to:"
echo "   Text: $OUTPUT"
echo "   Screenshot: ${OUTPUT%.txt}.png"
