# Agent notes

## No Unicode

Coding agents MUST NOT use Unicode (non-ASCII) characters anywhere in this
repository. This applies to:

- Markdown (`.md`) files
- Source code
- Source code comments

Use plain ASCII only. For example, use `->` instead of an arrow glyph, `x`
instead of a multiplication sign, `...` (three ASCII dots) instead of an
ellipsis character, and standard ASCII quotes rather than typographic quotes.

### Exception: HTML files

`.html` files MAY use Unicode glyphs where a real character is genuinely
needed (e.g. status/sensor badges such as a battery, lightning, or droplet
symbol). Source code, Markdown, and comments must still be ASCII-only.
