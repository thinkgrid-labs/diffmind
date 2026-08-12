# Example: Next.js App Router

A complete, working `.diffmind/` for a Next.js App Router project. Copy the
folder into your repo root and run `diffmind`.

```bash
cp -r examples/nextjs-app-router/.diffmind /path/to/your-next-app/
cd /path/to/your-next-app && diffmind rules list
```

## What is here

```
.diffmind/
├── rules.toml                        # regex rules — free, deterministic
└── rules/
    ├── react.md                      # medium · **/*.tsx, **/*.jsx
    ├── nextjs-app-router.md          # medium · app/**, src/app/**
    └── server-actions-security.md    # high   · **/actions.ts, **/route.ts, …
```

## The two kinds of rule, and which to use

|  | `rules.toml` | `rules/*.md` |
| --- | --- | --- |
| Matched by | regex, against added lines | the model, reading prose |
| Cost | nothing | tokens, on every review |
| Varies between runs | never | with the model |
| Use for | exact patterns | judgement |

**If a regex can decide it, put it in `rules.toml`.** `next/router` imported in
the App Router is always wrong — that is a pattern, and it should not cost a
token or depend on a model's mood. "Is this `'use client'` too high in the tree"
is a judgement, and belongs in a `.md`.

The 13 rules in `rules.toml` here catch: secrets behind `NEXT_PUBLIC_`,
`dangerouslySetInnerHTML`, Pages Router APIs that silently do nothing in `app/`
(`getServerSideProps`, `next/head`, `next/router`), raw `<img>` and `<a href="/">`,
index-as-key, `@ts-ignore`, and stray `console.log`.

## Scope is the important part

Every rule set that applies to a file is pasted into that file's prompt, and the
prompt has a byte budget it shares with the diff and the symbol context. **When
the rule sets do not fit, whole ones are dropped — quietly, in filename order.**

So the lever is not "write less", it is `scope`. These three are scoped so at
most two ever apply at once:

| Reviewing | Rule sets loaded |
| --- | --- |
| `app/dashboard/page.tsx` | `react` + `nextjs-app-router` |
| `app/api/users/route.ts` | `nextjs-app-router` + `server-actions-security` |
| `components/Button.tsx` | `react` |

Check yours with `diffmind rules list`, which prints each set's id, severity
ceiling and globs.

Two more things that follow from the budget:

- **A `severity:` ceiling stops one document dominating triage.** `react.md` is
  capped at `medium`, so a style opinion can never outrank a real security
  finding. Only `server-actions-security.md` is allowed to produce `high`.
- **Shorter rule sets review better.** These are ~1.5 KB each on purpose.
  Attention dilutes: twenty pages of rules makes the model worse at each one
  than two pages does.

## Suppressing a finding

```ts
// diffmind-ignore-next-line nextjs.raw-img
<img src={logo} />
```

Rule sets suppress the same way, under `rulebook.<id>`:

```ts
// diffmind-ignore-next-line rulebook.react
```

## Adapting this

Start by deleting what does not apply. A rule nobody agrees with gets
`diffmind-ignore`d everywhere within a week, and the whole file stops being read.

The `rules.toml` entries worth reviewing first for your project:

- `nextjs.public-env-key` — flags `NEXT_PUBLIC_*_KEY`/`_TOKEN` at `medium`
  because publishable keys legitimately use that prefix. Delete it if you have
  many; keep `nextjs.public-env-secret`, which is unambiguous.
- `nextjs.raw-img` and `nextjs.target-blank` — noisy in a codebase with a
  deliberate house exception. `target-blank` is `low` precisely because the
  regex crate has no lookahead, so it cannot check whether `rel` is present.
- `js.console-log` — delete if your linter already covers it.
