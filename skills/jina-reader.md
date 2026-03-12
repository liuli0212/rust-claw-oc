name: jina-reader
description: Extract clean, AI-friendly content from any webpage using Jina Reader API. Bypasses JavaScript restrictions and returns markdown format. Use when web_fetch fails or when you need clean content extraction from complex websites.

# Jina Reader

Extract clean, AI-friendly content from any webpage by prefixing URLs with `https://r.jina.ai/`.

## When to Use

- When `web_fetch` fails due to JavaScript requirements

- When you need clean markdown content from complex websites

- For extracting content from social media, news sites, or dynamic pages

- When dealing with sites that block traditional scrapers


## Usage

Simply prefix any URL with `https://r.jina.ai/` and use `web_fetch`:

 Original URL: https://example.com/article
 Jina Reader URL: https://r.jina.ai/https://example.com/article

## Examples

### Basic Usage

 # Extract from a news article
 web_fetch("https://r.jina.ai/https://techcrunch.com/article")

 # Extract from Twitter/X posts
 web_fetch("https://r.jina.ai/https://x.com/username/status/123456")

 # Extract from complex websites
 web_fetch("https://r.jina.ai/https://medium.com/@author/article")

### Social Media

 # Twitter/X posts
https://r.jina.ai/https://x.com/elonmusk/status/1234567890

 # LinkedIn posts
https://r.jina.ai/https://linkedin.com/posts/username-post-id

 # Reddit threads
https://r.jina.ai/https://reddit.com/r/subreddit/comments/thread_id

## Benefits

- **Bypasses JavaScript**: Works with dynamic content that requires JS

- **Clean output**: Returns structured markdown without ads or navigation

- **No authentication**: Works without API keys or login requirements

- **Universal**: Works with most websites including social media platforms

- **AI-optimized**: Content is formatted for AI consumption


## Limitations

- Requires internet access to Jina's service

- May not work with heavily protected or private content

- Rate limits may apply (though typically generous)

- Real-time content may have slight delays


## Error Handling

If Jina Reader fails:

1. Check if the original URL is accessible

2. Try the original `web_fetch` as fallback

3. Consider if the content requires authentication

4. Verify the URL format is correct


## Implementation

Use with existing `web_fetch` tool - no additional setup required:

 # Instead of:
 web_fetch("https://example.com/article")

 # Use:
 web_fetch("https://r.jina.ai/https://example.com/article")
