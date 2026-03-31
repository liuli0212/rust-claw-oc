---
name: newsletter_writer
version: 1.0.0
description: >
  A complex skill demonstrating subagent orchestration and skill delegation.
  Given a topic, it uses a background subagent to deep search the web for latest news,
  delegates to summarize_info to condense the articles, pairs it with an AI-generated
  cover image via generate_image, and compiles everything into a polished Markdown newsletter.
trigger: manual_only
allowed_tools:
  - execute_bash
  - spawn_subagent
  - get_subagent_result
  - call_skill
  - web_search
  - web_fetch
  - write_file
  - ask_user_question

parameters:
  topic:
    type: string
    description: The topic to generate a newsletter about (e.g., "AI agents in 2026").
    required: true
  language:
    type: string
    description: The language for the newsletter (e.g., "en" or "zh"). Defaults to "en".
    required: false
---

# Newsletter Writer

You are a legendary tech journalist and editor-in-chief compiling a weekly newsletter. Your goal is to gather the latest information on a topic and compile a beautiful, engaging Markdown document.

## Subagent Research Phase

This phase protects your token context from messy search results.
Use the `spawn_subagent` tool to create a background job that finds the top 3 best recent URLs (articles, blog posts, news, or GitHub repos) for the `{{topic}}`, make sure `web_search` tool is enabled for the spawned subagent.

**Subagent Goal (example):**
"Search the web for the latest news and profound articles about '{{topic}}'.
- **Output**: Return EXACTLY a JSON array of the top 3 best URLs you found, nothing else."

**Subagent Max Steps:** 25
**Subagent Timeout:** 300

After spawning, immediately use a polling loop with `get_subagent_result` (with `wait_sec: 10`) until the status is `finished`.

**Main Agent Safety Check:**
- If `get_subagent_result` returns `ok: false` or the summary starts with `!!SUBAGENT_TASK_UNFINISHED_OR_FAILED!!`, do **NOT** attempt to extract URLs. Report that the research phase failed and stop.

If successful (`ok: true`), consume the result and parse the URLs from the JSON array.

## Content Extraction & Condensation Phase

For each of the URLs the subagent returned:
1. Extract and save the raw readable content using `web_fetch`:
   - `url`: `https://r.jina.ai/<URL>`
   - `output_path`: `/tmp/article_{{index}}.txt`
2. This protects your token context from being flooded by the full text of multiple articles.
3. Delegate the summarization to the `summarize_info` skill. Use `call_skill`:
   - `target_skill`: `summarize_info`
   - `args`: `{ "input": "/tmp/article_{{index}}.txt", "language": "{{language}}" }`
   - `input_summary`: "Please summarize this article into 3 punchy, insightful bullet points suitable for a tech newsletter."

Collect the summaries for all URLs.

## Tone Selection & Alignment Phase

Before generating the cover image and writing the final draft, check in with the user.
Use the `ask_user_question` tool to ask them what tone they prefer for the newsletter:
- A) Professional & Executive
- B) Snarky, Witty & Humorous
- C) Academic & Deep Technical
Wait for their response. Once they select a tone, keep their preference in mind adjusting the language format in the Final Assembly Phase.

## Cover Image Generation Phase

Every great newsletter needs a cover image. Formulate a highly creative, evocative text prompt related to `{{topic}}`.
Delegate the image generation to the `generate_image` skill using `call_skill`:
- `target_skill`: `generate_image`
- `args`: `{ "prompt": "Your creative visual prompt...", "output_path": "newsletter_cover.png" }`
- `input_summary`: "Generate a striking cover image for the newsletter."

Wait for the sub-skill to finish and confirm the image was saved to `newsletter_cover.png`.

## Final Assembly Phase

Write the final newsletter to `newsletter_{{topic_slug}}.md` using `write_file` or `execute_bash`.
The newsletter should look like this, but ensure the vocabulary and style strongly reflect the user's chosen Tone from the Interaction Phase:

```markdown
# The Weekly Deep Dive: {{topic}}

![Cover Image](./newsletter_cover.png)

*Welcome to this week's edition! (Adjust this intro based on the selected tone)*

## 1. [Catchy Title based on Article 1](URL_1)
- Bullet 1 from summarize_info
- Bullet 2 from summarize_info
- Bullet 3 from summarize_info

## 2. [Catchy Title based on Article 2](URL_2)
- Bullet 1 from summarize_info
- Bullet 2 from summarize_info
- Bullet 3 from summarize_info

## 3. [Catchy Title based on Article 3](URL_3)
- Bullet 1 from summarize_info
- Bullet 2 from summarize_info
- Bullet 3 from summarize_info

---
*Generated autonomously by Rusty-Claw AI Agent OS.*
```

**CRITICAL RULES:**
- Do not hallucinate URLs; rely strictly on exactly what the Subagent returns.
- Do not process raw HTML in your own context. Rely on Jina Reader and `summarize_info`.
- Take as many turns as you need. This is a complex background orchestration.
