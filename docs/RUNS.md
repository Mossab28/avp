# What real runs taught us

AVP is used at [Murmell](https://murmell.com), where it runs our own repository and has
drastically improved how our agents ship code. It is still being tested internally, and it will
be released to the public in Murmell.

Every defect a real run finds goes here: the symptom, the cause, the fix. AVP gets better one run
at a time, and this log keeps the trace of what broke and why.

## On our own repository

| # | Symptom | Cause | Fix |
|---|---|---|---|
| 1 | Timing tests failed for no reason | A proof ran beside a full suite on the same machine | One check at a time per machine, then isolated workers |
| 2 | False reds from a project the previous run left behind | The server's home directory persisted from one proof to the next | A fresh HOME and TMPDIR for every run |
| 3 | A change the CI's security scan refused got through | The proof did not run the scan | The scan is a proof step, same version and rules as the CI |
| 4 | Changes to infrastructure files and workflows were proven in seconds | `{files}` only lists code, so no step saw them | `{all_files}` for the scan, and the CI scripts' tests when a task touches `.github/` |
| 5 | The CI on `main` never caught up (9 runs, 144 jobs interleaved) | One full CI per landing, each in its own group | One run at a time on `main`, only the newest waits |
| 6 | Two CI tests broke after fix 5 | They checked the old rule, and proving a `.yml` ran no test | Tests updated; the CI scripts' tests run whenever `.github/` changes |
| 7 | Two database migrations were ordered under ones already applied | The migration-order step only received code, not `.sql` files | Migrations renamed; the step takes `{all_files}` |
| 8 | Shards of the first full suite ran on the server instead of a worker | New worker machines took longer to start than the 120 s a job waits for one | The wait is 15 minutes (`AVP_WORKER_CLAIM_S`) |
| 9 | A staging deploy started by AVP was cancelled | The CI started staging too and took its place in the queue | Staging only follows AVP's full suite |
| 10 | A full suite went red on a browser test, green on the same code just before | A test that fails under load | A red shard runs again once; green the second time, it is marked flaky |
| 11 | The server's password showed in the workers' logs | The full URL was printed | Printed as `https://user@host`, never the password |
| 12 | With `AVP_VIEWS` set, an agent worked outside the folder `avp up` lets it write | The MCP server `avp up` registers did not receive `AVP_VIEWS` | `avp up` passes it |
| 13 | Lines of the MCP journal mixed when two agents work at once | Each entry went out in several writes | One write per line |
| 14 | Every agent started with two errors (`avp_events`, `avp_tasks`) | The instructions send it there first, and those tools needed an open task | The read tools work without a task |
| 15 | Two Codex agents out of three never saw AVP and left their changes in the checkout | Codex does not pass an MCP server's instructions to its model; the rule in the tool descriptions was not enough | `avp up` writes the rule in an untracked `AGENTS.md`, kept out of `git status` |
| 16 | An environment was reported as serving a new version while one reading still returned the old one | During a rolling deploy, old and new instances answer in turn | An environment serves a commit once four readings in a row agree |
| 17 | No `avp/line` branch in the checkout, although `avp up` said there would be | Without a remote, the mirror only wrote its commits in its own worktree | Without a remote, the mirror moves the local branch, fast-forward only, never the one checked out |
| 18 | An AVP agent cost twice what a Git agent cost (15 to 24 turns against 5 to 6) | Half the benchmark: a tool allowlist on the AVP side that a shell hook bypassed. Half the protocol: `avp_tasks` and `avp_events` first, `avp_check` before `avp_land` | Same permissions on both sides; `avp_open` says who else is working, and the rule is "open, write, land" |

## On a repository that is not ours (go-chi/chi)

`avp up` took 31 s (50 commits imported, `go vet` and `go test` guessed and landed). Three Claude
Code agents (Sonnet), never told about AVP: all three went through it, with no Git commit. Two of
them changed the same function at the same time; AVP replayed the second on the first, with no
markers and a correct result (398 tests green). The same three tasks on plain Git ended in a
conflict on two files, settled by a fourth agent.

The same three tasks with Codex: 1 agent in 3 went through AVP at first, 0 in 3 with the rule in
the tool descriptions (defect 15), and 3 in 3 with the rule in an untracked `AGENTS.md`. An agent
takes AVP as soon as it sees the tools and one sentence of rule where it reads its instructions,
whatever the model.

One agent at a time, the three tasks in a row, same permissions on both sides: 0.3.2 took 233 s
and $0.347 through AVP against 151 s and $0.332 through Git, because each landing ran the tests
again after the agent had run them itself. 0.3.4 starts the proof while the agent tests: 146 s
and $0.330, and every landing reused that proof.

Two providers at once, 0.3.9: Claude Code and Codex changed the same function at the same time.
Both landed, Codex's replayed on Claude's with no conflict, then a second Claude task on another
file. The head holds both changes, the tests are green, and the checkout has no Git commit.

## `avp up` on one public repository per language

No agent: `avp up`, then one task opened, written and landed through the CLI.

| Language | Repository | Guessed | Landing |
|---|---|---|---|
| Node (pnpm) | unjs/defu | tsc, vitest on the task's files | 1 s |
| Python (uv) | pallets/itsdangerous | ruff, pytest | 1 s |
| Rust | dtolnay/itoa | cargo check, cargo test | 10 s |
| Go | google/uuid | go vet, go test | 2 s |
| Java (Maven) | jhy/jsoup | mvn test | 9 s |
| Java (Gradle) | gradle/gradle-build-scan-quickstart | gradlew test (the repository's build cache serves the proofs) | 0 s |
| Ruby (rspec) | piotrmurach/tty-color | rspec, read from the `.gemspec` | 1 s |
| Ruby (rake) | ruby/rake | refused, rightly: 4 of the repository's own tests fail on Ruby 4 | |
| Python (poetry) | python-poetry/cleo | pytest (ruff is only configured there, not installed: defect 27) | 1 s |
| Node (pnpm monorepo) | t3-oss/create-t3-app | the project's `typecheck` (turbo, every package) | 2 s |
| Node (npm, mocha) | expressjs/express | the project's `test` script | 2 s |

| # | Symptom | Cause | Fix |
|---|---|---|---|
| 19 | Python with uv: no check landed | The setup installed the project in a folder that only has `pyproject.toml` and `uv.lock`, and the build backend wants the README and the sources | The setup installs the dependencies alone; the project installs in each candidate |
| 20 | After that failure, no mirror and no MCP: `avp up` stopped | The setup error went all the way up | A setup that fails is a guess to fix: `avp up` says so and goes on |
| 21 | Ruby: `bundle install` refused | A Homebrew Ruby's gem folder is not writable | Gems go to `~/.avp/bundle` (or `$BUNDLE_PATH`) |
| 22 | Ruby: nothing guessed for a gem tested with rspec | rspec is declared in the `.gemspec`, which `avp up` did not read | `avp up` reads `.gemspec` and `.rspec` |
| 23 | One agent on AVP took 233 s against 151 s on Git for three tasks | At landing, it waited for the tests to run again, right after running them itself | The proof starts after 3 s without a write, while the agent tests; landing reuses it, and a proof of an older tree is stopped |
| 24 | A server could make a client write outside the task's folder | Paths were checked for writes, not for imports nor when a tree is laid out | Every path is checked on the server and in the client: no empty, `.` or `..` part, nothing absolute |
| 25 | A change to a stylesheet was proven green, then the CI failed on two tests that read that stylesheet as text | Related tests were found through imports, and nothing imports a stylesheet | A test that names a touched file (stylesheet, fixture, migration) is related to it and goes into `{files}` |
| 26 | `avp serve --listen 0.0.0.0` would have opened the code to anyone who could reach it | The server had no authentication of its own | `AVP_TOKEN` (the name in the URL is the person), and a refusal to listen beyond this machine without a token or a proxy |
| 27 | Python with poetry (cleo): the guessed checks were red on the head | A `[tool.ruff]` section made ruff a step, but the project runs ruff through pre-commit and never installs it | ruff is a step only when the project lists it as a dependency |
| 28 | A pnpm monorepo (create-t3-app): a type error in a package would have landed green | The step was `tsc -p .` at the root, whose tsconfig covers two config files | The project's own `typecheck` script first (turbo here), else every package's; the same type error is now refused |
| 29 | Where proofs go to workers, as on our repository, the early proof was off: every landing waited for the whole proof (5 min for an engine change) | A proof already sent to a worker could not be stopped, so a newer tree would have waited for it | A newer tree stops the older proof: out of the queue if it waits, and its worker hears it at its next sign of life (every 2 s) and kills it. The early proof runs on workers, never on the server |
| 30 | A reader: "humans still need to review diffs, roll back and audit" | People had the Git mirror and `avp why`, but no way to read a position as a change, nor to undo one | `avp show <position>` (goal, author, agent, proof, full suite, commit, then the diff) and `avp revert <position>`: a task that undoes it on the head, merged with what changed since, proven before it lands. On chi, reverting a change another agent had built on was refused by the proof: it removed an import the later test needed |
| 31 | A reader: "your commit log needs the agent id, or blame stops working" | The agent was in the commit message (`AVP-Agent`), not in the author: `git blame` showed the person only | The author is `person (agent)` with the person's email, so blame names the agent on every line, and an `AVP-Session` trailer tells two sessions of the same agent apart |
| 32 | `avp why` said who wrote a file, rarely how: 5 tasks in 58 on our repository had a link to the agent's conversation | The agent had to pass it to `avp_open` itself, and seldom did; and a file is too coarse when several tasks wrote it | `avp why <file>:<line>` follows the line back to the task that wrote it, and `avp_open` finds the conversation Claude Code or Codex is writing on its own |
| 33 | Our whole suite went red on a position whose code was fine: `EACCES` copying a tracked image into a build folder | A proof folder was reused from one shard to the next and kept what earlier proofs wrote; tracked files were hard links to read-only objects, so a copy of one stayed read-only; and a dependency added since never reached a reused folder | One folder per slot, laid out like a fresh checkout before every proof: what is not in the tree goes, tracked files are writable copies (clones where the disk can), laid again when a step changed them, and the setup's links are made again when the setup changed. The disk now holds one folder per slot instead of one per task |
| 34 | AVP's own crate: once the folder was laid out fresh before every proof, every proof compiled the whole crate again (28 to 30 s) | `target` went with the rest of what a step leaves | `keep` in `[check]`: folders kept from one proof to the next like a CI cache, `["target"]` for Cargo by `avp up`. The same proofs take 10 s. And when the guessed checks are red on the head, `avp up` says which step failed and its last line (`sh: cargo: command not found`), and abandons the task it opened |
| 35 | Gabriel's agent: "AVP makes a folder per agent, not optimal" | Every new agent session laid the whole tree out in a new folder, one request per file (more than two minutes for our 3,592 files), with no install, and every sync read and hashed every file | A session takes a folder another session left free (a holder file says which live session has one), made the head again first: a file a task changed without sending it is laid again, a file no task landed goes, what `.gitignore` hides stays. Files the agent's checkout already holds are copied, the rest come in batches, four at a time. A sync reads only the files whose size, times or inode moved. On our repository: 11.2 s for a first folder (from a server without batches), 1.0 s for the next session, its install kept |
| 36 | Production deployed from GitHub's button instead of `avp promote`: AVP saw it serve position 88, and still showed our promotion to 80 as being deployed | A promotion stayed asked for until the environment served exactly it | A promotion the environment went past since, by other means, is no longer asked for: a bridge carrying it out would have taken production back to 80. One made after the environment moved is a rollback, and stays |
| 37 | Gabriel's proofs on our repository took 7 minutes (median of 16), against 15 s for ours: his tasks touch central modules, and vitest relates 77 to 324 test files to them. One red came after 13 minutes | A proof ran its steps one after the other, in one job | `split` on a step: AVP counts the related tests through the imports and runs the step as that many shares side by side, `{shard}` as `i/n`, beside one job for the other steps; the first red part stops the others, on the workers too. A worker claims a part only if it says it knows parts, and fetches a tree it lacks by batches. On four local workers: the red share back in 0.5 s instead of after the others' 30 s |
| 38 | Landing a task on our repository from a laptop: the connection was cut after 271 s, while the proof ran | Nothing went through the connection while the proof ran, and a load balancer or a NAT drops a connection it takes for idle | A client that sends `x-avp-keepalive` gets a space every 15 s while a land or a check is worked out, then `{"status", "body"}`; any other client gets the plain answer and its status. The CLI and the MCP server send it |
| 39 | Our staging served position 108 for more than half an hour while positions 109 to 121 went green | Every green full suite promoted staging again, while the deploy before was still running: each new deploy took the place of the last, and staging never settled | An environment that follows the full suite takes the newest green position once it serves the one it was promoted to (or once that deploy has gone unserved for 30 minutes), and at once when its deploy is observed |

## Still open

- The full suite on our workers has matched our CI on every verdict so far; we keep both running
  and compare over days.
