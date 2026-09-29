# Account lifecycle, social approval, and instance controls

This document describes the behavior added for:

- [#26 Protected accounts](https://github.com/csd113/RustPost/issues/26)
- [#27 Instance-wide announcements](https://github.com/csd113/RustPost/issues/27)
- [#28 Maintenance mode](https://github.com/csd113/RustPost/issues/28)
- [#29 Account export and import](https://github.com/csd113/RustPost/issues/29)
- [#31 Admin forced password reset](https://github.com/csd113/RustPost/issues/31)
- [#32 Admin forced logout](https://github.com/csd113/RustPost/issues/32)
- [#33 Account deletion grace period](https://github.com/csd113/RustPost/issues/33)
- [#34 Username change and username history](https://github.com/csd113/RustPost/issues/34)

All state is stored in the SQLite database (schema version 4) so it survives
restarts and is covered by instance backups. New routes use the existing
server-rendered forms, CSRF tokens, and no-JavaScript fallbacks.

## Protected accounts (#26)

- Each account has a persistent `Require approval for new followers`
  preference on the settings page (`users.follow_approval_required`).
- Following a protected account creates a row in `follow_requests` and sends
  the owner a `follow_request` notification. A pending request is **not** a
  follow: it does not appear in follower counts or lists, does not grant feed
  privileges, and does not create a `follows` row.
- The owner sees incoming requests at `/follow-requests` and can **Approve**
  or **Reject** them. Approving creates the follow and notifies the requester
  (`follow_request_approved`). Rejecting removes the request without a
  notification. Requesters can cancel their own pending request from the same
  page, from the profile button ("Requested"), or by unfollowing.
- Enforcement: self-follows are rejected; suspended or deleted targets cannot
  be followed; a block in either direction removes pending requests in both
  directions and prevents new ones; duplicate and concurrent requests are
  idempotent through composite primary keys.
- Enabling protection **preserves existing followers**. Disabling protection
  leaves already-pending requests pending — the owner can still approve or
  reject them, and new follows are accepted immediately. Nothing is silently
  auto-approved.
- Pending requests are cleaned up when the requester or target account is
  deleted.

## Instance announcements (#27)

- Administrators manage the announcement on the admin dashboard
  (`/admin`, "Announcement" panel).
- An enabled, non-empty announcement renders in the top bar next to the
  RustPost/site name. On narrow screens it wraps onto its own line under the
  brand row.
- Announcement text is limited to 280 characters, escapes control characters
  (newlines and tabs are allowed), and is HTML-escaped when rendered.
- Stored in the `instance_settings` table, so it survives restarts, backup,
  and restore. An empty or disabled announcement renders nothing.
- The "Clear announcement" button disables and empties the announcement in one
  step.

## Maintenance mode (#28)

- Toggled by administrators on the admin dashboard (`/admin`, "Maintenance
  mode" panel) with an optional message (280 characters, HTML-escaped).
- While enabled, these state-changing routes return `503` with the maintenance
  message:
  - `POST /register` (registration) — for everyone.
  - `POST /posts` (new posts **and** replies) — except administrators.
  - `POST /posts/{id}/edit` — except administrators. Editing is blocked so a
    draft written before maintenance cannot be published by rewriting it.
  - `POST /posts/{id}/quote` — except administrators.
  - `POST /posts/{id}/repost` — except administrators.
  - `POST /settings/import` (importing an archive publishes posts) — for
    everyone except administrators.
- The policy lives in one table (`maintenance_policy` in `src/server.rs`) with
  a table-driven test covering every publish route, method, and role, so new
  routes cannot silently bypass maintenance checks.
- Administrator exemption: administrators can still post, reply, quote,
  repost, edit, and import so they can verify the instance. They also keep full
  access to the admin UI to disable maintenance mode. Registration is not
  exempt because new accounts are not part of instance verification.
- The site stays readable: all `GET` pages, logins, sessions, likes,
  bookmarks, follows, blocks, mutes, account settings, password changes,
  exports, and account deletion flows keep working. Maintenance mode does not
  silently disable security or account operations.
- The composer/reply form is not rendered for visitors who cannot post, and
  every page shows a maintenance banner with the configured message.

## Account export and import (#29)

RustPost account archives are separate from full-instance backups. Export is
owner-only (`GET /settings/export`); import writes into the authenticated
destination account (`GET`/`POST /settings/import`) and never creates accounts.

### Archive format (version 1)

A gzip-compressed tar archive (`.tar.gz`) containing:

| Entry | Contents |
| --- | --- |
| `manifest.json` | format version, app name, creation timestamp, random archive id, archive handle |
| `profile.json` | display name, bio, location, website, theme, NSFW blur, liked-posts visibility, follow-approval preference |
| `posts.json` | the account's posts, replies, and quotes with archive-local ids and remapped parent/root/quote references |
| `media.json` | media metadata (kind, MIME type, filename, byte length, alt text, NSFW flag, SHA-256) |
| `media/...` | media bytes for every entry in `media.json` (yes, bytes are included) |
| `follows.json` | outgoing follows as portable handle references with timestamps |
| `settings.json` | muted words |

Limits: 300 MiB compressed per archive (upload and export output), 1 GiB of
total decompressed media, 10,000 entries, 5,000 posts, 1,000 media, 5,000
follows, 500 muted words. The compressed and expanded ceilings are independent:
raising the upload limit never raises the decompressed limit, and extraction
enforces the expanded limit incrementally. Operators can tune all three archive
ceilings in `settings.toml` under `[accounts]`:

```toml
[accounts]
max_archive_upload_bytes = 314572800      # compressed, also exported size
max_archive_expanded_bytes = 1073741824  # total decompressed media bytes
max_archive_entries = 10000
```

Media bytes are hashed and verified during import, and declared media metadata
(byte length, SHA-256, MIME type/kind) must agree with the actual extracted
bytes; unrecognized or mismatched media content is rejected.

### Never included

Password hashes, sessions, CSRF tokens, delete-intent tokens, other secrets,
administrator flags, suspension/deletion state, forced password-reset state,
instance configuration, other accounts' posts/media, notifications, likes,
bookmarks, reposts, and account-import records for the exported account.

Import summaries report what was skipped: follow targets that are missing,
deleted, suspended, self, or blocked, duplicate/pending follow states, and
reply/quote references that could not be remapped. A post whose media list
references media that is not part of the archive is rejected during validation
instead of silently importing an incomplete post.

### Import rules

- The destination account is always the authenticated account that performs
  the import. Archive contents can never claim another account, change the
  destination username, grant administrator status, resurrect sessions, or
  clear forced-reset/deletion restrictions.
- The complete archive is validated before any database or filesystem
  mutation: format version, JSON schema, per-record limits, path safety
  (no absolute paths, `..`, backslashes, encoded traversal, non-ASCII or
  case-colliding entry names, links, device files, or duplicates), media
  hashes/lengths/content type, and both archive size ceilings.
- Structural limits are enforced incrementally while the gzip/tar stream is
  read: entry count, per-entry declared size, total decompressed media bytes,
  and cumulative JSON document bytes. A tiny compressed archive can never
  expand into unbounded memory or disk.
- The upload is streamed to a staging file with the compressed limit applied
  for every chunk; an oversized archive gets a `413` page naming the configured
  limit instead of a generic transport failure.
- Media files are staged and installed before the database transaction
  commits; failures remove copied files, and the whole import is recorded in
  `account_imports` so re-importing the same archive into the same account is
  rejected instead of duplicating content.
- Profile fields (display name, bio, location, website) fill only empty
  destination fields; non-empty destination values win and are counted as
  skipped. Theme, NSFW blur, liked-posts visibility, and follow-approval
  preference are applied from the archive. Usernames are reported but never
  changed.
- Posts are inserted with remapped ids. Reply/quote relationships to posts
  that are not part of the archive are dropped. Reposts and likes/bookmarks of
  other accounts' posts are not portable and are not imported.
- Follows resolve by canonical handle on the destination instance:
  - existing, active accounts become follows;
  - protected accounts become **pending follow requests** — import never
    bypasses approval;
  - missing, deleted, suspended, self, or blocked accounts are skipped and
    counted in the import report.
- JSON fields such as `is_admin`, `is_suspended`,
  `must_change_password`, `deletion_scheduled_at`, or `password_hash` are
  ignored; they can never synthesize privileged or restricted destination
  state.
- Media bytes are copied into the destination uploads directories with new
  unique names. If an identical canonical media file already exists on the
  destination, the import records a duplicate row pointing at the existing
  file instead of copying bytes again.
- Imports are blocked for non-administrators while maintenance mode is active
  and share the posting rate-limit budget. Accounts that are restricted to a
  forced password change or pending deletion cannot start an import at all, so
  an archive can never lift those restrictions.

### Nonportable data

Reposts, likes, bookmarks, notifications, sessions, passwords, administrator
privileges, private/deleted account state, other accounts' content, instance
configuration, and Tor keys are not ported by account archives. Follows are
resolved against accounts that already exist on the destination instance;
RustPost is non-federated and does not invent remote accounts.

## Admin forced password reset (#31)

- `POST /admin/users/{id}/require-password-reset` sets
  `users.must_change_password` and writes an audit row.
- After that, every authenticated request (including existing sessions) is
  redirected to `/settings/password?required=1` until the password changes.
  The only reachable flows are the password change, logout, account deletion,
  and account export, plus static assets. If the account state cannot be read,
  the request fails closed instead of continuing.
- The password page renders only the password form and a logout button while
  the restriction is active.
- Passwords are validated with the existing policy and hashed with the
  existing Argon2id mechanism. The flag is cleared in the same database
  transaction that stores the new hash, and all other sessions are revoked
  exactly as in a normal password change.
- Repeating the admin action is harmless; a forced logout (`#32`) does not
  clear the requirement.

## Admin forced logout (#32)

- `POST /admin/users/{id}/revoke-sessions` marks every active session of the
  selected account revoked and writes an audit row. The action reports how many
  sessions were revoked.
- Revoked cookies stop authenticating on the next request across all protected
  routes; other accounts' sessions are unaffected, and the account can log in
  again unless another restriction (suspension, pending deletion) applies.
- Repeated revocation is safe: it simply reports zero remaining sessions.

## Account deletion grace period (#33)

- Configuration: `[accounts] deletion_grace_period_days` in `settings.toml`,
  default **30 days**, maximum 3650, `0` means immediate deletion after
  password confirmation. Documented in the generated `settings.toml`.
- The delete flow (`/settings/delete` → `/settings/delete/confirm`) verifies
  the password and stores `deletion_requested_at` and `deletion_scheduled_at`
  instead of deleting immediately. The user sees the deadline and a cancel
  button on the settings page, plus a site-wide banner while the deletion is
  pending.
- Access policy while a deletion is pending: the account can still log in,
  read, export its archive, change its password, log out, and cancel the
  deletion. Publishing (posts, replies, quotes, reposts), imports, profile
  edits, and social actions are refused with a clear message until the
  deletion is cancelled.
- Permanent removal uses persisted timestamps and runs from the periodic
  maintenance task (also on startup), so it survives restarts and does not
  depend on an in-memory timer. Finalization is idempotent and re-checks the
  deadline and cancellation inside one transaction, so concurrent cancellation
  wins safely.
- Finalization runs the same scrubbing logic as password-confirmed deletion:
  posts, reposts, likes, bookmarks, follows, blocks, mutes, follow requests,
  notifications, reports, sessions, rate-limit rows, uploaded media owned by
  the account, and shared media references are handled exactly as before.
  Media still referenced by other accounts is preserved.
- Username reservations are released on permanent deletion: `username_history`
  rows for the account are removed so the handles become available again. This
  is a deliberate policy choice for a small, self-hosted instance; historical
  handles of active accounts stay reserved.
- Deletion records anonymous release tombstones (`released_username:<handle>`
  rows in `instance_settings`) for the handles it frees. A handle that no
  account owns renders a tombstone page instead of a bare 404, and if someone
  registers a released handle, the new profile is marked as a different
  account. This keeps the username-release policy while ensuring an old
  profile URL can never silently masquerade as the deleted account.
- Finalization writes a durable pending-media-deletion journal before the
  database transaction commits and removes files afterwards, deleting the
  journal only when every file is gone. A crash between commit and file
  removal is recovered from the journal at startup, so the process is
  restart-safe and idempotent without depending on in-memory state.
- Database/filesystem failures during finalization are logged, leave the
  account intact for retry, and never leave a partially deleted account.

## Username changes and history (#34)

- `POST /settings/username` (settings page "Username" panel) requires the
  current password. New handles are validated with the shared username rules
  (allowed characters, length, reserved names) and canonicalized
  case-insensitively.
- Uniqueness is enforced atomically in one transaction against both the
  current `users.normalized_username` values and `username_history`, so
  concurrent changes and registrations cannot claim the same handle.
- The previous handle is recorded in `username_history` with a timestamp.
  Internal identity (user id), posts, follows, permissions, sessions, blocked
  lists, exports, and deletion state are all keyed by user id and are
  unaffected.
- Another account can never claim a handle that appears in history. The same
  account may switch back to a handle it previously used; the stale history
  row for that handle is replaced by the newer previous name.
- Profile pages show a "Previously known as @…" note for accounts with
  history. Requesting a handle that no account currently owns but that has
  history renders a "no account uses this handle" page listing the previous
  holders with links — old profile links and `@mentions` never silently
  resolve to a different person or redirect to a new owner.
- A handle released by permanent deletion becomes claimable again, but the
  deletion records an anonymous release tombstone. Old URLs render a
  "released handle" page while the handle is unclaimed, and a profile that
  claims a released handle is explicitly marked as a different account, so an
  old link can never appear to be the deleted account.
