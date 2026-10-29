Version control is git, on GitHub, with a merge queue protecting main, as many teams run it. This folder
is your clone; its `origin` is the shared GitHub repository and `main` is the line everyone delivers to;
`gh` is logged in. Start each task from the latest main (`git pull --rebase origin main`) on a branch of
its own (`git switch -c <short-name>`). Nobody can push to main directly.
To deliver a task: commit it, run `pnpm typecheck` and `pnpm test`, push the branch
(`git push -u origin HEAD`), open a pull request (`gh pr create --fill --base main`) and put it in the
merge queue (`gh pr merge --squash`). CI runs the typecheck and all the tests on the pull request, then
again in the queue on main plus the changes queued ahead of yours; the queue merges yours only when that
passes. The task is delivered once its pull request is merged: you can go on with your next task and check
back (`gh pr view <number> --json state,mergeStateStatus`, `gh pr checks <number>`; not more than every
30 s). If the queue or the CI turns it down (a conflict with main, or a failing check), read why, pull
main with rebase, fix, push the branch again and put it back in the queue; when the failure comes from a
teammate's work that is not on main yet, wait a little and try again. Leave `.github/` as it is.
