# Gacha Plugin

Genshin Impact wish simulator with accurate pity mechanics.

## Per-player pools

Every caller gets their own pool and pity, keyed by their QQ id (from the
kernel-injected `__meta`); CLI/ops calls use the `default` player. State lives
in `data/gacha/users/<uid>.json`.

The pool a player pulls on is the **default banner** — a built-in copy of the
real Genshin rate-up (`banner.json` overrides it) — unless that player set their
own with `setbanner`. The legacy `data/gacha/state.json` still seeds the
`default` player until it is written once.

## Permissions

- `setbanner` by an operator (`data/qq_operators.json`, or a CLI invoke with no
  `__meta`) changes the **default** banner and the operator's own pool.
- `setbanner` by anyone else changes **only their own** pool. A player cannot
  target another player: identity comes from `__meta`, and an explicit `user`
  argument is honoured for operators only.

## Counting featured vs off-banner

`pull` / `status` return `stats.featured_4_count` and
`stats.off_banner_4_count`. Use those. Counting by character name is wrong: the
standard 4★ pool contains the featured names, so an off-banner draw can have the
same name as a featured one.

Note the 4★ "featured" share is **75%** overall (50% per pull, plus a guarantee
after a non-featured 4★), not 50/50.

## Nodes

- **gacha_entry** — Pull wishes (`pull`), inspect or set the pool (`banner`,
  `setbanner`), compare with the roster (`list`), reset pity (`reset`).
- **gacha_status** — Check current pity counters and guarantee state.

## Historical banners

`data/gacha/banners.json` records past rate-ups:

```json
[{ "id": "5.3-1", "version": "5.3", "phase": 1, "from": "2025-01-01", "to": "2025-01-21",
   "featured_5": ["玛薇卡", "茜特菈莉"], "featured_4": ["班尼特", "迪奥娜", "菲米尼"],
   "source": "https://…" }]
```

- `search <关键词>` matches id, version, dates and character names.
- `setbanner <id>` applies a recorded banner to your own pool (an operator applies
  it to the default pool).
- `history_add` writes an entry and is **operator-only**: a player must not be
  able to seed fake history that everyone then searches. Characters are validated
  against the roster, so a recorded banner is always pullable.
- The file starts empty. Populate it with `history_add` (keep the `source` URL);
  an agent can fill it from its own web search when an operator asks.
