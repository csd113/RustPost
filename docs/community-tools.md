# Drafts, conversations, and reliable posting in 1.2.0

These features are prepared on `dev` for 1.2.0. Check the published release page before downloading; a version bump does not publish a release.

## Save or schedule a post

Sign in, write a post or reply, attach media, and choose **Save draft**. Open **Drafts** on any device signed into the same account to continue. Draft text, attachment descriptions, and scheduling are stored on the server. Draft-only attachment URLs require the owner's session. Administrators with database/filesystem access can read drafts; bytes already shared in a public post remain public even if reused in a draft.

Use the scheduling field and **Schedule** to publish later. All date/time fields are explicitly **UTC**. Scheduling requires a future time within 366 days. The server must be running: after downtime, overdue posts are considered on startup and then every five seconds. Publication is transactional and cannot create a second post after a restart. Rate limits defer a scheduled post, and maintenance pauses scheduled publication. Invalid or restricted posts remain in Drafts with a failure message for correction. Deleting a reply target cannot turn a saved reply into a new root post.

Cancel a schedule to return it to an editable draft, or delete it. Editing uses a revision check, so a stale device cannot silently overwrite another device's changes. Each account can keep 200 unpublished drafts/schedules. A successful save/publish wins if it races cancellation; inspect the resulting status before trying again.

The composer carries a random submission key. Retrying the same form after a lost response returns the committed post and does not consume another posting-rate allowance. A fresh composer has a new key and can intentionally publish the same text. Reloading a form after an uncertain response starts a new submission, so check your profile first. This is request deduplication, not text-based duplicate detection.

## Describe images

Selecting images shows local previews and a separate description field for each image, up to 1,000 characters. Describe the relevant content rather than repeating surrounding post text. Saved drafts retain descriptions, which can be reviewed beside existing attachments. Without JavaScript, use the per-line description field in the same attachment order.

The optional missing-description reminder is stored only in the browser's local preferences; descriptions themselves are saved on the server. The reminder can be bypassed when appropriate and does not block saving an unfinished draft. Local previews use browser object URLs and do not send image bytes anywhere before submission.

## Polls

Open the composer's poll controls, enter 2–6 distinct choices (one per line, up to 100 characters each), and choose single or multiple selection. An optional UTC close time stops voting. Polls require a signed-in author and voter. A voter can replace their selection while voting remains open; a multi-choice vote is saved atomically. Published choices and voting mode are fixed.

Results show counts and your selection, without a list of voters. **Votes are not anonymous to the site operator:** account IDs and selections are stored in SQLite. Site backups contain them. Blocking and account restrictions apply to voting.

## Find older content

Home offers **Everyone** and, when signed in, **Following**. Following includes your own posts and approved followed accounts. The selection stays in the URL. Profiles, bookmarks, feeds, and search have older/newer navigation with stable cursors; conversations have earlier/later reply pages and links back to the root or parent. Direct links to late replies continue to open their context. Pages are a live view, so content newly added or removed between requests can change the result.

Search accepts ordinary text plus these filters, combined with spaces:

| Filter | Meaning |
| --- | --- |
| `from:alice` | Posts by the current username `alice` |
| `after:2026-11-01` | On or after this UTC date |
| `before:2026-12-01` | Before this UTC date |
| `has:media`, `has:image`, `has:video` | Attachment type |
| `is:bookmarked` | Your bookmarks; requires sign-in |

For example, `garden from:alice after:2026-11-01 has:image`. Search uses the existing local SQLite index; no search service is required. Matching account results have their own navigation. Blocks, mutes, private likes, and deleted content retain their normal visibility rules.

## Background media processing

Accepted post attachments become usable in their original format before optional FFmpeg conversion. **Media processing** shows your uploads' state and allows retries after failures. A single worker accepts at most 128 pending/running conversions, makes up to three automatic attempts with backoff, and stops after nine total attempts including explicit retries. Restart recovers interrupted work without discarding originals. Conversion publication is atomic with the media record, and deleting an upload cannot leave a worker publishing a stale result.

FFmpeg remains optional. Missing encoders or conversion failure leave an allowed original available; browser codec support varies. Profile images/banners still use the existing bounded upload-time conversion. Successful conversions follow `media.keep_original_uploads`. Identical files can share a conversion while retaining each upload's description and visibility rules.

## Undo your own deletion

Deleting your own post immediately hides it. Open **Recently deleted** to restore it during the configured recovery window (30 seconds by default). The exact UTC deadline is shown. Attachments are retained until the deadline and are private to the owner while no other public post references them. Restore does not send another notification or recreate a profile pin.

After expiry, cleanup runs in the background. Operator downtime can delay physical cleanup but cannot extend the restore deadline. Administrator removals are final, including an administrator removing a post already waiting for its owner's undo. Account deletion is a separate process.

## Operator controls and upgrade

Back up the complete site before upgrading. Startup migrates supported older SQLite schemas to **schema 5** in one transaction, preserving existing posts/media. Existing settings receive safe defaults for new fields. Downgrading requires the matching older binary and a pre-upgrade full-site backup; an older binary cannot use schema 5. Backup format **1** and account archive format **1** are unchanged.

Full-site backups include drafts, schedules, polls/votes, media work, and recovery state. Account export/import remains a selected-content format: it does **not** transfer drafts, schedules, poll definitions/votes, or deletion recovery. Exported poll post text/media can transfer without its poll. Use a full-site backup to preserve these features.

New settings, editable in **Admin → Deep server settings**, take effect after restart:

```toml
[storage]
min_free_bytes = 536870912      # 512 MiB; 0 disables filesystem headroom checks
max_upload_bytes = 0           # instance logical upload quota; 0 = unlimited
max_user_upload_bytes = 0      # per-account logical quota; 0 = unlimited

[posts]
post_delete_recovery_seconds = 30 # 0 disables undo; maximum 300
```

Add keys to existing sections rather than duplicating them. **Admin → Storage** shows active limits, free space, uploaded bytes, and reserved uploads. Your **Media processing** page shows your own usage. Quotas count media input bytes, including drafts, retained deletions, and duplicate media records; filesystem free-space probes separately check physical files, conversion outputs, backups, and staging. Reservations make concurrent uploads/imports respect logical quotas. **Free-space checks do not reserve physical capacity or guarantee a minimum free-space floor.** Concurrent uploads, conversions, backup creation, and verification can each pass against the same available bytes and then collectively consume the headroom. External programs can also exhaust disk space; keep host monitoring and off-host backups. Some filesystems cannot report free space reliably; setting headroom to zero disables that probe deliberately.

Backup capture freezes the SQLite snapshot and its referenced immutable media, then resumes application requests before hashing/archiving. Hard links make capture faster; unsupported filesystems use a copy fallback. Backups remain serialized. After interruption, the capture marker is recovered only after acquiring exclusive native backup ownership; a live owner or an unexpected/nonempty marker keeps requests paused and readiness at 503. **Verify** in Admin → Backups and `verify-backup ARCHIVE` perform full validation and rehearse migration/path rewriting in temporary storage without replacing the live site. See the [operator guide](operator-guide.md#backup-and-restore).

`GET /health/live` returns a minimal liveness response. `GET /health/ready` checks the database with a two-second bound and returns 503 during capture/busy/unavailable states. Both use `Cache-Control: no-store` and reveal no configuration or content. Readiness does not prove external Tor reachability, free disk capacity, or FFmpeg support.

YouTube playback sends only this site's origin to the embedded YouTube player using an iframe-specific referrer policy; the page-wide `same-origin` policy and no-referrer external thumbnails/links remain. Privacy extensions, enterprise policies, proxies that rewrite headers, and Tor Browser restrictions can still prevent external embeds. Opening the external link remains available.

Use a canonical absolute data-directory path without `..` components. The existing safe-deletion checks reject media paths containing parent components; resolve such paths before first use.
