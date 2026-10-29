# Task: invoices

Build the Invoices domain of Facturo exactly as SPEC.md describes it (section "Invoices"):
`src/invoices.ts` with `register(app)`, the types in `src/types.ts` next to `Invoice`, the
routes wired in `createApp` (`src/app.ts`), and tests in `test/invoices.test.ts` using the
in-process client. The `paidCents` / `dueCents` fields of `GET /invoices/:id` belong to the
Payments teammate: expose a hook or leave them at 0 / totalCents, but do not implement payments.
Two teammates build Customers and Payments at the same time, in the same repository:
`src/app.ts` and `src/types.ts` are shared, touch them as little as the work needs.
Done means: `pnpm typecheck` and `pnpm test` pass, every rule of the Invoices section is
covered by a test.
