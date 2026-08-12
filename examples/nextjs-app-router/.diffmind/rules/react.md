---
description: React correctness, hooks discipline and accessibility
id: react
scope: ["**/*.tsx", "**/*.jsx"]
severity: medium
---

# React review standards

Judgement calls only — anything a regex can decide is in `.diffmind/rules.toml`.

- **Derived state must be computed, not stored.** A `useState` whose only writer
  is a `useEffect` mirroring a prop will desynchronise. Compute it during render.
- **An effect that subscribes, opens or times something needs a cleanup.**
  Missing teardown is a leak.
- **Dependency arrays must be honest.** A value read inside the effect and left
  out of the deps is a stale-closure bug. If the array was trimmed to stop a
  loop, the loop is the real problem — usually an object or function recreated
  each render.
- **Work that belongs to a user action belongs in the handler**, not in an
  effect that fires on mount.
- **A list key must identify the item, not its position.** Index keys corrupt
  state when the list reorders or filters.
- **Memoisation needs a reason.** Flag both directions: a missing memo on
  something expensive in a hot list, and a pointless one around a string concat.
- **A context value built inline** (`value={{ user, setUser }}`) re-renders every
  consumer on every parent render.
- **Accessibility**: interactive elements reachable by keyboard (a `<div onClick>`
  needs role, `tabIndex` and a key handler — or just use `<button>`), images have
  `alt`, inputs have a real label rather than only a placeholder.
