// LGUI-07: A tiny, dependency-free "just enough" markdown-ish parser for Lumina chat replies.
// Deliberately NOT a markdown library — spec §3.2 calls for minimal, injection-safe rendering,
// not full CommonMark. This module only ever produces plain-data tokens (strings + a tag
// discriminant); it does no HTML string-building and is imported by nothing that calls
// `dangerouslySetInnerHTML`. `ChatBubble.tsx` renders every token as a React element/text node,
// so an assistant reply containing literal HTML (e.g. `<script>...</script>`) can only ever
// reach the DOM as inert text — there is no code path in this file or its consumer that turns
// untrusted string content into markup. See `chatMarkdown.test.ts` for the XSS-proof assertion.
//
// Supported inline spans: **bold**, `inline code`, [label](url) (http/https only — any other
// scheme, including `javascript:`, degrades the whole span to plain text rather than becoming
// a live link). Supported block form: fenced ``` code blocks; everything else is one paragraph
// of inline tokens. That's the whole feature set — anything not matched is passed through as
// literal text, never dropped and never interpreted as markup.

export type InlineToken =
  | { kind: 'text'; value: string }
  | { kind: 'bold'; value: string }
  | { kind: 'code'; value: string }
  | { kind: 'link'; label: string; href: string };

export type Block =
  | { kind: 'paragraph'; tokens: InlineToken[] }
  | { kind: 'codeblock'; value: string; lang: string | null };

const SAFE_LINK_SCHEME = /^https?:\/\//i;

// Order matters: code spans first (so `**not bold**` inside backticks stays literal), then
// bold, then links. Each alternative is captured so we know which branch matched.
const INLINE_RE = /`([^`]+)`|\*\*([^*]+)\*\*|\[([^\]]+)\]\(([^)]+)\)/g;

/** Parses one line/paragraph's worth of text into an ordered list of inline tokens. Never
 *  throws; unmatched input always becomes `{kind:'text'}` with the original substring intact
 *  (including any literal `<`, `>`, `&` — this function does no escaping because it never
 *  produces markup for those characters to hide inside). */
export function parseInline(text: string): InlineToken[] {
  const tokens: InlineToken[] = [];
  let last = 0;
  INLINE_RE.lastIndex = 0;
  let m: RegExpExecArray | null;
  while ((m = INLINE_RE.exec(text)) !== null) {
    if (m.index > last) {
      tokens.push({ kind: 'text', value: text.slice(last, m.index) });
    }
    const [whole, code, bold, linkLabel, linkHref] = m;
    if (code !== undefined) {
      tokens.push({ kind: 'code', value: code });
    } else if (bold !== undefined) {
      tokens.push({ kind: 'bold', value: bold });
    } else if (linkLabel !== undefined && linkHref !== undefined) {
      if (SAFE_LINK_SCHEME.test(linkHref)) {
        tokens.push({ kind: 'link', label: linkLabel, href: linkHref });
      } else {
        // Unsafe/unknown scheme (javascript:, data:, vbscript:, bare text, …) — render the
        // original bracket syntax back out as inert text rather than ever wiring it to `href`.
        tokens.push({ kind: 'text', value: whole });
      }
    }
    last = m.index + whole.length;
  }
  if (last < text.length) {
    tokens.push({ kind: 'text', value: text.slice(last) });
  }
  return tokens;
}

/** Splits a full reply into paragraph/code-block blocks on ``` fences. A dangling/unclosed
 *  fence is treated as a code block running to the end of the string (never silently dropped). */
export function parseBlocks(text: string): Block[] {
  const blocks: Block[] = [];
  const parts = text.split(/```/);
  parts.forEach((part, i) => {
    const isCodeBlock = i % 2 === 1;
    if (isCodeBlock) {
      const firstNewline = part.indexOf('\n');
      const lang = firstNewline === -1 ? null : (part.slice(0, firstNewline).trim() || null);
      const value = firstNewline === -1 ? part : part.slice(firstNewline + 1);
      blocks.push({ kind: 'codeblock', value, lang });
    } else if (part.length > 0) {
      blocks.push({ kind: 'paragraph', tokens: parseInline(part) });
    }
  });
  return blocks;
}
