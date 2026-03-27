---
name: jina_reader
description: Extract clean, AI-friendly content from any webpage using Jina Reader API. Bypasses JavaScript restrictions and returns markdown format.
trigger: suggest_only
allowed_tools: [web_fetch]
---
# Jina Reader

Extract clean, AI-friendly content from any webpage by prefixing URLs with `https://r.jina.ai/`.

## When to Use

- When `web_fetch` fails due to JavaScript requirements
- When you need clean markdown content from complex websites
- For extracting content from social media, news sites, or dynamic pages
- When dealing with sites that block traditional scrapers

## Usage

Simply prefix any URL with `https://r.jina.ai/` and use `web_fetch`:

```
# Original URL: https://example.com/article
# Jina Reader URL: https://r.jina.ai/https://example.com/article
```

## Examples

```bash
# Extract from a news article
web_fetch("https://r.jina.ai/https://techcrunch.com/article")

# Extract from Twitter/X posts
web_fetch("https://r.jina.ai/https://x.com/username/status/123456")
```

## Limitations

- Requires internet access to Jina's service
- May not work with heavily protected or private content
- Rate limits may apply (though typically generous)
