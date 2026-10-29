# Task: customers

Build the Customers domain of Facturo exactly as SPEC.md describes it (section "Customers"):
`src/customers.ts` with `register(app)`, its types in `src/types.ts` next to `Customer`, the
routes wired in `createApp` (`src/app.ts`), and tests in `test/customers.test.ts` using the
in-process client. Two teammates build Invoices and Payments at the same time, in the same
repository: `src/app.ts` and `src/types.ts` are shared, touch them as little as the work needs.
Done means: `pnpm typecheck` and `pnpm test` pass, every rule of the Customers section is
covered by a test.
