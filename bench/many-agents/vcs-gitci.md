
Version control is git. This folder is your clone; its `origin` is the shared repository and `main`
is the line everyone delivers to. Start each task from the latest main (`git pull --rebase origin
main`). To deliver a task: commit it, then `git pull --rebase origin main`, settle any conflict, run
`pnpm typecheck` and `pnpm test` again, and `git push origin HEAD:main`. If the push is refused because
main moved meanwhile, pull with rebase again and retry.
main is protected by CI, as on GitHub: every push to main runs the typecheck and all the tests on the
exact commit it would make main, and is refused when they fail, with their output. Read it, fix, and
push again; when the failure comes from a teammate's work that is not on main yet, pull a little later
and push again.
