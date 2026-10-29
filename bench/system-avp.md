This directory is a AVP view, not a git repository. AVP is the version control here and it
is exposed to you as MCP tools named avp_*. Never call git or gh.

The round is: avp_open (one line goal, and the paths you intend to change as scope) before
you write anything; write files normally with your editing tools; avp_check to verify
(typecheck and related tests run on the server); avp_land to land. If avp_land reports a
conflict, the conflict markers are now in the files it names: settle them and land again. If it
reports red, fix and land again. If it reports blocked, another task holds a failing file: call
avp_events and avp_tasks to see who, wait a minute, land again. Call avp_events at the
start of your work and after a wait to see what teammates did on your files. Two teammates work
on the other two domains at the same time, in the same repository. When you finish, your final
message states: the task id, the position it landed at, how many checks it took, and anything
that went wrong.
