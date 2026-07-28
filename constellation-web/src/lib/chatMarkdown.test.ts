// LGUI-07: self-checking coverage for `chatMarkdown.ts`, following the dependency-free
// convention established by `commandMatch.test.ts` (this repo has no JS test runner wired up
// yet — see that file's doc comment; not this item's job to add one). Typechecks as part of
// `tsc --noEmit`; run directly via `npx tsx src/lib/chatMarkdown.test.ts`.
//
// The last two checks ARE the LGUI-07 XSS proof: an assistant reply containing a literal
// `<script>` tag must come back as a single inert `{kind:'text'}` token carrying the tag as
// plain characters — never split/interpreted/dropped, and never any token kind that
// `ChatBubble.tsx` would render via markup injection (there is no such kind — `link`/`bold`/
// `code` all render their `value`/`label` as text content too, but this asserts the parser
// itself never even classifies script-shaped input as something special).
import { parseInline, parseBlocks } from './chatMarkdown';

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) throw new Error(`chatMarkdown.test: ${message}`);
}

export function runChatMarkdownTests(): number {
  let passed = 0;
  const check = (name: string, fn: () => void) => {
    fn();
    passed += 1;
  };

  check('parseInline: plain text with no spans is a single text token', () => {
    const tokens = parseInline('hello world');
    assert(tokens.length === 1, `expected 1 token, got ${tokens.length}`);
    assert(tokens[0].kind === 'text' && tokens[0].value === 'hello world', 'unexpected token');
  });

  check('parseInline: bold span', () => {
    const tokens = parseInline('say **hi there** now');
    assert(tokens.length === 3, `expected 3 tokens, got ${tokens.length}`);
    assert(tokens[1].kind === 'bold' && tokens[1].value === 'hi there', 'bold token wrong');
  });

  check('parseInline: inline code span', () => {
    const tokens = parseInline('run `cargo test` please');
    const code = tokens.find(t => t.kind === 'code');
    assert(code !== undefined && code.kind === 'code' && code.value === 'cargo test', 'code token wrong');
  });

  check('parseInline: code span wins over bold-looking content inside it', () => {
    const tokens = parseInline('`**not bold**`');
    assert(tokens.length === 1, `expected 1 token, got ${tokens.length}`);
    assert(tokens[0].kind === 'code' && tokens[0].value === '**not bold**', 'should stay literal inside code');
  });

  check('parseInline: https link renders as a link token', () => {
    const tokens = parseInline('see [the docs](https://example.com/x)');
    const link = tokens.find(t => t.kind === 'link');
    assert(link !== undefined && link.kind === 'link' && link.href === 'https://example.com/x', 'link token wrong');
  });

  check('parseInline: javascript: scheme link degrades to inert text, never a link token', () => {
    const tokens = parseInline('[click me](javascript:alert(1))');
    assert(tokens.every(t => t.kind !== 'link'), 'javascript: scheme must never produce a link token');
    const joined = tokens.map(t => (t.kind === 'text' ? t.value : '')).join('');
    assert(joined.includes('javascript:alert(1)'), 'original text must be preserved verbatim as inert text');
  });

  check('parseInline: data: scheme link also degrades to inert text', () => {
    const tokens = parseInline('[x](data:text/html,<script>alert(1)</script>)');
    assert(tokens.every(t => t.kind !== 'link'), 'data: scheme must never produce a link token');
  });

  check('parseBlocks: fenced code block extracted with language', () => {
    const blocks = parseBlocks('before\n```js\nconsole.log(1)\n```\nafter');
    const code = blocks.find(b => b.kind === 'codeblock');
    assert(code !== undefined && code.kind === 'codeblock' && code.lang === 'js', 'lang not parsed');
    assert(code!.kind === 'codeblock' && code!.value.trim() === 'console.log(1)', 'code value wrong');
  });

  // ── XSS proof (LGUI-07 acceptance criterion) ────────────────────────────────

  check('XSS PROOF: a literal <script> tag in plain text parses as one inert text token', () => {
    const evil = 'hi <script>alert(1)</script> bye';
    const tokens = parseInline(evil);
    assert(tokens.length === 1, `expected the whole string as one text token, got ${tokens.length}`);
    assert(tokens[0].kind === 'text', 'must classify as text, never a markup-producing kind');
    assert(tokens[0].kind === 'text' && tokens[0].value === evil, 'must preserve the tag verbatim, unescaped-but-inert');
  });

  check('XSS PROOF: a <script> tag inside a paragraph block round-trips as text tokens only', () => {
    const blocks = parseBlocks('Here is something odd: <script>alert(1)</script> — inert.');
    assert(blocks.length === 1 && blocks[0].kind === 'paragraph', 'expected one paragraph block');
    const para = blocks[0] as Extract<typeof blocks[0], { kind: 'paragraph' }>;
    assert(
      para.tokens.every(t => t.kind === 'text' || t.kind === 'bold' || t.kind === 'code' || t.kind === 'link'),
      'no token kind exists that would let ChatBubble inject raw markup',
    );
    const rendered = para.tokens.map(t => ('value' in t ? t.value : t.label)).join('');
    assert(rendered.includes('<script>alert(1)</script>'), 'the literal tag text must survive intact for inert rendering');
  });

  return passed;
}

const results = runChatMarkdownTests();
// eslint-disable-next-line no-console
console.log(`chatMarkdown self-check: ${results} assertions passed`);
