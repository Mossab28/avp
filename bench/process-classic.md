
# Process (classic)

You are in a git worktree on your own branch, created from `main`. Two teammates work on the
other two domains on their own branches at the same time. Commit as you go. When your domain
is done and `pnpm typecheck` and `pnpm test` pass locally: push the branch, open a pull request
with `gh pr create`, wait for the CI to be green (`gh pr checks --watch`), then bring your branch
up to date with `main` if it moved (`git fetch origin && git rebase origin/main`, settle
conflicts, `git push --force-with-lease`, wait for the CI again), and merge with
`gh pr merge --squash --delete-branch`. A merge is only allowed on a green CI run of the exact
head you merge. After the merge, your work is done. If CI fails because of your code, fix and
push again. If the final merged `main` would break because of a teammate's work, that is theirs
to fix: say so and stop. Your final message states: the PR number, the merge commit, how many CI
runs your PR took, how many rebases, and anything that went wrong.
