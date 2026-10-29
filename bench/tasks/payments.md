# Task: payments

Build the Payments domain of Facturo exactly as SPEC.md describes it (section "Payments"):
`src/payments.ts` with `register(app)`, the types in `src/types.ts` next to `Payment`, the
routes wired in `createApp` (`src/app.ts`), the `paidCents` / `dueCents` fields on
`GET /invoices/:id`, the outstanding report, and tests in `test/payments.test.ts` using the
in-process client. Two teammates build Customers and Invoices at the same time, in the same
repository, and your work depends on theirs: if their routes are not there yet when you test,
create the invoices your tests need through the store directly, and wire the real routes when
they land. `src/app.ts` and `src/types.ts` are shared, touch them as little as the work needs.
Done means: `pnpm typecheck` and `pnpm test` pass, every rule of the Payments section is
covered by a test.
