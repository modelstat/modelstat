# Chrome Web Store listing — modelstat extension

Everything you need to paste into https://chrome.google.com/webstore/devconsole when submitting.

## Basic info

- **Extension name**: `modelstat — AI token spend tracker`
- **Summary** (132 chars max): `Track token usage and cost across ChatGPT, Claude.ai, Gemini, and Grok. Metadata only — your prompts never leave your browser.`

## Description (long — up to 16,000 chars, but 500-1,000 is the sweet spot)

```
modelstat tracks AI token spend across the web chat interfaces your team already uses — ChatGPT, Claude.ai, Gemini, and Grok — and unifies it with your Claude Code, Cursor, Codex, and other AI coding tool usage in one dashboard.

⸻ What it does ⸻

The extension captures only the token metadata each site already displays — model name, input/output tokens per turn, conversation ID, timestamps. It never reads your message contents, never transmits the prompts or responses you type or receive, and never accesses other tabs.

Combined with the modelstat desktop agent (macOS/Linux), you get one dashboard showing spend across every AI tool your team uses: terminal agents (Claude Code, Codex, Cline, Aider), editors (Cursor, Continue, Windsurf, Zed, Copilot), desktop apps (Claude Desktop), and web chat (ChatGPT, Claude.ai, Gemini, Grok).

⸻ What the extension captures ⸻

• Model name (e.g. GPT-5, Claude Opus 4, Gemini 2.5 Pro)
• Input, output, and reasoning token counts per message
• Cache-creation and cache-read split (Anthropic)
• Multimodal breakdown (text/image/audio/video) where the page exposes it
• Provider-assigned conversation ID
• Timestamps

⸻ What the extension does NOT capture ⸻

• Your messages
• The model's responses
• File contents or attachments
• Any data from tabs outside the four supported providers
• Browser history, bookmarks, saved passwords, cookies

⸻ How it works ⸻

The extension is manifest-v3 with strict host permissions — it only runs on chatgpt.com, claude.ai, gemini.google.com, grok.com, and x.com/i/grok. It reads only the page metadata each provider already exposes to its own UI. Adapter configs are signed with an Ed25519 key the extension verifies on download, so a compromised server can't push a malicious adapter.

The source is auditable at github.com/modelstat/modelstat.

⸻ Setup ⸻

1. Install the extension.
2. Sign in (OAuth, one click).
3. Use ChatGPT / Claude.ai / Gemini / Grok as normal — the extension captures metadata in the background.
4. Visit your dashboard at modelstat.ai/dashboard to see the breakdown.

Pair with the desktop agent (curl -fsSL https://install.modelstat.ai | sh) to capture Claude Code, Cursor, and other local tool usage in the same dashboard.

⸻ Pricing ⸻

• Free tier: 100M tokens/month, no credit card.
• Team plan: $5 per seat per month with 250M pooled tokens included.

⸻ Privacy ⸻

Full privacy policy at modelstat.ai/privacy. Summary: we never see the content of your conversations, we never access other browser tabs, we log only the metadata your provider already shows you.

⸻ Support ⸻

Issues: github.com/modelstat/modelstat/issues
Email: hello@modelstat.ai
```

## Category

**Productivity** (primary). **Developer Tools** as secondary if the store allows.

## Language

English

## Screenshots (5, 1280x800)

| # | What | Notes |
|---|---|---|
| 1 | Extension popup showing recent spend | Pulled from modelstat.ai logged in, clean personal data |
| 2 | ChatGPT tab with the extension badge active | Shows which page is captured |
| 3 | Claude.ai tab with the badge | Same |
| 4 | Dashboard page showing web-chat spend rolled up alongside Claude Code | The "unified view" money shot |
| 5 | Privacy settings screen in the extension popup | Shows "we capture only X, never Y" |

## Promo tiles

- **Small promo** (440x280): Brand mark + "Track AI token spend. Metadata only." 
- **Marquee** (1400x560): Same but wider, with screenshots flowing behind

## Permissions justification (required at submission)

**storage**: Persisting the user's auth token and local captured-events buffer. No content stored.

**alarms**: Periodic upload of buffered events to the modelstat API. No background data collection beyond what's triggered by the four supported host permissions.

**scripting**: Injecting a single main-world script per supported host (chatgpt.com, claude.ai, gemini.google.com, grok.com, x.com/i/grok) to read token metadata the page exposes to its own UI. No data read from other sites.

**host_permissions** (chatgpt.com, claude.ai, gemini.google.com, grok.com, x.com/i/grok): The extension runs only on these five hosts. No `<all_urls>`. No permissions that would allow reading data from other sites.

**No remote code**: All adapter logic is bundled in the extension. Adapter configs downloaded at runtime are pure data (JSONPath selectors + DOM queries), verified against a bundled Ed25519 public key before use.

## Privacy policy URL

https://modelstat.ai/privacy

## Support URL

https://modelstat.ai/install

## Website URL

https://modelstat.ai

## After submission

- Review turnaround is typically 1-5 business days
- If rejected, the usual reason is permission justification — submit-revise-resubmit is fine
- Once live, claim the extension on the site with a "Add to Chrome" badge that auto-deeplinks to the correct store URL
