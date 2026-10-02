# Project-level intent alias config (`.graphtell/aliases.json`)

graphtell's recall relies on a "Chinese query term → English identifier" bridge. The built-in
[`INTENT_ALIASES`](../../crates/gt-application/src/recall_service.rs) only keeps **generic words
that most likely hold in any project** (CRUD verbs, order/pay/user, sms/merge/spec …), and
deliberately contains no business-specific jargon (e.g. "flash sale", "group buy", "team buy").

The right home for business jargon is **each codebase's own** `.graphtell/aliases.json`: it travels
with the project, doesn't pollute the generic tool's source, and needs zero changes when you switch
projects.

## Location

Put it under the **project root** (the `root_path` shown by `graphtell list`):

```
<project-root>/.graphtell/aliases.json
```

At recall time the tool reads this file by `root_path` and **merges** it with the built-in table
before expanding query terms. (Note: if several projects share one `root_path`, they share this
config.)

## Format

A JSON object: keys are Chinese intent words, values are arrays of the English tokens they map to
(tokens are matched as **substrings** against node names / fqn):

```json
{
  "秒杀": ["seckill"],
  "拼团": ["combination", "pink"],
  "佣金": ["brokerage", "commission"]
}
```

## Merge semantics

- **Merged** with the built-in `INTENT_ALIASES`: words from the project config are appended to the
  recall dictionary.
- **Same-name keys append rather than override**: if the project writes `"订单": ["seckill_order"]`,
  the result is `订单 → [order, seckill_order]` -- the built-in `order` is not dropped.
- **Missing / unparsable file**: silently falls back to the built-in table (logging one `warning`),
  never blocking recall.

## How to find the tokens to fill in

1. Grep the codebase for domain nouns and see what English the class names / method names actually
   use (e.g. CRMEB's `StoreSeckillServices` → `seckill`; litemall's `LitemallGroupon` → `groupon`).
2. Run `graphtell recall --query "你的中文问题" --markdown` once and look at the Chinese words and
   English expansions listed under **"missed concepts"** in the quality assessment -- those are what
   you should add.

## Examples

A few reference examples ship in this repo:

- CRMEB: `samples/php-projects/thinkphp/CRMEB/.graphtell/aliases.json`
- Bagisto: `samples/php-projects/laravel/bagisto/.graphtell/aliases.json`
- litemall: `samples/java-projects/litemall/.graphtell/aliases.json`
