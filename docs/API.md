# AVP Cloud API: accounts, organizations, plans

The routes of a server with `AVP_CLOUD=1` (see [GUIDE.md](GUIDE.md#avp-cloud-organizations-and-plans)),
for a web page or an agent. JSON in, JSON out. A request carries a person's token
(`Authorization: Bearer avp_...`) or AVP Live's session cookie; a refusal is `{"error": "..."}` with
401 (no token), 403 (no right), 404 (missing, or not yours: the two read the same), 400 (anything
else), 402 (proof minutes used up) or 429 (too many sign-ups or sign-ins from the address).
Without `AVP_CLOUD=1`, the organization routes answer 400.

## Repos and their spaces

A repo lives in a space: an organization, or its creator's own (their name). Its name is unique
there, and it is said `<space>/<name>`; the repo's JSON carries both (`{"id", "name": "shop",
"space": "acme", "org": "acme", ...}`). Every route that takes `{repo}` takes `acme/shop`
(`/repos/acme/shop/line`, or `/repos/acme%2Fshop/line`), the repo's id, or a name alone: the
asker's repo of that name; 409 when several of theirs have it, the error listing them. The access
check stays per repo.

| Route | Who | Body / answer |
|---|---|---|
| `POST /repos` | any person | `{"name": "shop"}` in their own space, or `{"name": "acme/shop"}` in an organization they administer; `"policy"` as before → the repo. 400 `acme/shop already exists: choose another name` when the space has one |
| `GET /{space}/{repo}.git/info/refs` | readers | `git clone https://<server>/acme/shop.git` |

## Accounts

`POST /signup` (with `AVP_SIGNUP=open`), no token:

```json
{"name": "ada", "email": "ada@example.com", "password": "at least 10 characters"}
→ {"user": {"name": "ada", "email": "ada@example.com", "github": null, "admin": false, "created_at": 1793000000000, "created_by": "sign-up"},
   "token": {"token": "avp_...", "id": "tok_...", "person": "ada", "name": "first", "kind": "token", "created_at": 1793000000000, "expires_at": null, "revoked_at": null, "used_at": null}}
```

Sent with `x-avp-live: 1`, it sets AVP Live's session cookie too.

`POST /session`, no token: `{"name": "ada" or "ada@example.com", "password": "..."}`. With
`x-avp-live: 1`: the session cookie and `{"person": "ada"}`; otherwise a token of 90 days:
`{"person": "ada", "token": {...as above}}`. `{"token": "avp_..."}` still signs AVP Live in with a
token. `DELETE /session` signs out.

`GET /auth` → `{"accounts": true, "github": true, "signup": true, "password": true}`: how one signs
in here. `GET /auth/github` starts GitHub's sign-in; with sign-ups open, a GitHub login no one has
makes the account.

`GET /me` → `{"accounts": true, "person": "ada", "admin": false, "via": "token", "github": true,
"cloud": true, "roles": [{"repo": "acme/shop", "role": "admin"}], "orgs": [{"org": "acme", "role": "owner", "plan": "free"}]}`.
`roles` holds the role each repo gives, its own or the organization's.

## Organizations

| Route | Who | Body → answer |
|---|---|---|
| `GET /orgs` | anyone signed in | → `[{"name", "plan", "role", "seats", "created_at"}]`, theirs (a server admin: all, `role` null where not a member) |
| `POST /orgs` | anyone signed in | `{"name"}` → `{"name", "plan": "free", "created_at", "created_by"}`; the creator is `owner` |
| `GET /orgs/{org}` | members | → `{"name", "plan", "role", "seats", "members": [{"person", "role"}], "repos": ["shop"], "invites": [invite], "created_at", "created_by"}`; `invites` for admins only |
| `POST /orgs/{org}/invites` | admins | `{"to": "bob" or "bob@example.com", "role": "member"}` → invite |
| `POST /orgs/{org}/members` | admins; a member for themselves | `{"person", "role": "member"\|"admin"\|"owner"\|"none"}` → `{"org", "person", "role", "was"}` |
| `POST /orgs/{org}/repos` | admins of the org and of the repo | `{"repo": "shop"}` (the asker's own `shop`, or `ada/shop`, or an id) → the repo, with `"org": "acme"`; 400 when the org has a repo of that name |
| `POST /orgs/{org}/plan` | server admins | `{"plan": "free"\|"team"\|"enterprise"}` → the org |
| `GET /orgs/{org}/usage` | members; a person for their own name | → usage |
| `GET /orgs/{org}/events?since=&limit=` | admins | → `[{"id", "at", "kind": "org.member", "data": {"org", "person", "role", "was", "by"}}]` |
| `GET /invites` | anyone signed in | → `[invite]` waiting for them, by name or by their email |
| `POST /invites/{id}` | the invited | `{"accept": true}` → `{"org", "role", "accepted"}` |
| `GET /repos/{repo}/usage` | readers of the repo | → usage of its space, or `{"space": null}` |

An invite: `{"id": "inv_...", "org": "acme", "person": "bob" or null, "email": null or "bob@example.com", "role": "member", "by": "ada", "at": 1793000000000}`.

A usage, for the calendar month (UTC):

```json
{"space": "acme", "kind": "org", "plan": "free", "month": "2026-11",
 "minutes": 2012.4, "proof_minutes": 1890.1, "full_minutes": 122.3,
 "cap": 2000.0, "over": true, "resets_at": "2026-12-01T00:00:00Z",
 "seats": 3, "repos": ["shop", "web"],
 "said": "acme used 2012.4 of its 2000 proof minutes this month (free plan): ..."}
```

`kind` is `person` for a person's own repos; `cap` is null on team and enterprise; `said` is there
only when `over`. Past the cap, `POST /repos/{repo}/tasks/{task}/check`, `.../land`,
`.../speculate` and `POST /repos/{repo}/full` answer 402 with `said` as the error.

Objects: `GET`, `HEAD /repos/{repo}/objects/{hash}` and `POST /repos/{repo}/objects` answer 404 for
an object the repo does not hold, whatever other repo holds it.
