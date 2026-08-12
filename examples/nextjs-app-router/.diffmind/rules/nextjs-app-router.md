---
description: App Router server/client boundary, caching and routing
id: nextjs-app-router
scope: ["app/**", "src/app/**"]
severity: medium
---

# Next.js App Router standards

## Server/client boundary

- **`'use client'` is a boundary, not a file annotation.** Everything it imports
  ships to the browser. Flag it added to a layout or page when only a leaf needs
  interactivity — push the directive down.
- **Server components cannot use** `useState`, `useEffect`, `useRouter`,
  `window`, `localStorage` or event handlers. Client components cannot be
  `async`.
- **Do not `useEffect`-fetch what a server component could fetch.** That pattern
  adds a round trip, a loading state and a waterfall.
- **Props crossing the boundary must be serialisable** — a function or class
  instance fails at runtime.

## Data and caching

- Independent `await`s in sequence are a waterfall; use `Promise.all`.
- **Caching changes deserve scrutiny in both directions.** `force-dynamic` or
  `no-store` quietly sends every request to the origin; caching something
  user-specific leaks one user's data to another. Anything depending on cookies,
  headers or the session must not be statically cached.
- A mutation that never calls `revalidatePath`/`revalidateTag` leaves stale data
  on screen.

## Rendering and routing

- Prefer `loading.tsx` or `<Suspense>` over a hand-rolled `isLoading` flag.
- `error.tsx` must be a client component and should offer `reset()`.
- Time, randomness or `window`-derived values on first paint will
  hydrate-mismatch — move them into an effect or behind a mounted check.
- Treat `params` and `searchParams` as untrusted user input.
- A new public page without `metadata`/`generateMetadata` ships with no title.
- Prefer `next/image` and `next/link` over bare `<img>` and `<a>` for internal
  routes; prefer `next/font` over a stylesheet font URL.
