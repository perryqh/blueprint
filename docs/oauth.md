# GitHub OAuth and the owner role

Anonymous browser commenters always work — they always have. What OAuth adds is
**provenance**: each comment is tagged with a server-stamped `role`, and only
one specific role triggers a plan edit from the agent. There are three roles:

| Role    | Identified by                                                               | Triggers a plan edit? |
| ------- | --------------------------------------------------------------------------- | --------------------- |
| `owner` | logged-in GitHub session whose login matches `BLUEPRINT_OWNER_GITHUB_LOGIN` | **Yes**               |
| `user`  | any other logged-in session (or the CLI/agent bearer)                       | No — reply only       |
| `guest` | anonymous browser commenter (no session)                                    | No — reply only       |

In legacy mode (no OAuth env), everything lands as `guest` and there is no
owner; the Claude skill treats every comment as a potential plan edit, same as
before the owner-role change. Configure OAuth before relying on
"only the owner steers."

## Configure

Populate `~/.blueprint/env`:

```ini
# ~/.blueprint/env
GITHUB_CLIENT_ID=Iv1.xxxxxxxxxxxxxxxx
GITHUB_CLIENT_SECRET=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
BLUEPRINT_OWNER_GITHUB_LOGIN=your-github-login   # case-insensitive
```

Sessions are cookie-backed and stored in `~/.blueprint/blueprints.db` (mode
`0600` — the session id is a bearer credential, so the file is owner-only).
There is no `SESSION_SECRET`: nothing signs the cookie, the random session id
*is* the secret. Sessions survive a daemon restart, which is what keeps the
OAuth round-trip and the owner role intact when the daemon respawns mid-flow.

`BLUEPRINT_OWNER_GITHUB_LOGIN` is **optional**. If you set it without the two
`GITHUB_*` vars, the daemon prints a startup WARN — owner-role assignment
depends on the OAuth session, so the env var is dead config without it. Same
the other way: enable OAuth without setting the owner, and you'll get a
different WARN noting that no comment will trip a plan edit.

## Callback URL and port

The daemon's default callback URL is
`http://127.0.0.1:7321/auth/github/callback`, and it must match the callback
registered on your GitHub OAuth app exactly. So with the defaults, the daemon
has to bind port 7321 for the round-trip to work.

To run on another port, change all three together: bind the new port
(`--port` / `BLUEPRINT_PORT`), set `OAUTH_CALLBACK_URL` in `~/.blueprint/env`
to match, and update the callback URL on the GitHub OAuth app:

```ini
# ~/.blueprint/env
OAUTH_CALLBACK_URL=http://127.0.0.1:7400/auth/github/callback
```

To drop back to anonymous mode, delete `~/.blueprint/env` (or unset the two
`GITHUB_*` vars) and restart the daemon.

## How the write gate interacts

Comments themselves have no 401 gate: anonymous ones land as `guest`, and
provenance via the `role` tag is the defense — that's what makes drive-by
review work. Destructive requests are gated, though. Once OAuth is configured,
creating, replacing, or deleting a blueprint and `POST /api/shutdown-if-empty`
all require a session or the CLI bearer, and answer 401 without one
(`WriteKind::Blueprint` in `src/auth.rs`). With no OAuth configured the daemon
stays in legacy local-trust mode and enforces nothing, since that's the only
way the CLI works before credentials exist.

See also **Roles and the write gate** in the [README](../README.md#how-it-works).
