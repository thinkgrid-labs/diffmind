---
description: Server Actions are public endpoints — authz, validation, data leaks
id: server-actions-security
scope: ["**/actions.ts", "**/actions/**", "**/route.ts", "**/*.server.ts"]
severity: high
---

# Server Actions and data-access security

## A Server Action is a public HTTP endpoint

`'use server'` does not mean internal. Every exported action becomes a POST
endpoint with a stable ID that **anyone can call directly**, with any arguments,
regardless of what the UI allows. In every action:

- **Authenticate inside the action.** A check in the page, layout or middleware
  does not protect it.
- **Authorise the specific object.** "Logged in" is not "may edit *this* record".
  An action taking an `id` must verify ownership.
- **Validate every argument** with a schema — types are erased at runtime.
- **Never trust a client-supplied `userId` or hidden form field.** Read identity
  from the session, server-side.

Flag any exported `'use server'` function that reaches a database or external
service before doing all four.

## Client bundle leaks

- `NEXT_PUBLIC_*` values are inlined into JavaScript the browser downloads. A
  key, token or secret with that prefix is a disclosed credential.
- A secret read in a module imported by a client component leaks even if never
  rendered — the import graph decides, not the usage.
- Treat a new import of a database client or admin SDK into a `'use client'`
  file as a leak.

## Data returned to the client

- **Select fields explicitly.** Returning a whole user row ships the password
  hash, the email and everything added to that table later. This is the most
  common real data leak in this framework.
- Errors sent to the client must not carry stack traces, queries or connection
  strings.

## Route handlers

- `route.ts` has none of a page's protections: same auth, authorisation and
  validation burden as an action.
- Cookie-authenticated mutating handlers need CSRF consideration.
- A redirect target built from `searchParams` is an open redirect unless checked
  against an allowlist.
