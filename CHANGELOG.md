# Changelog

## v1.0.0 - First Release

### Runtime and Dependency Baseline
- Raised the minimum supported Rust version to `1.91` and updated local, CI, and release-build documentation accordingly.
- Upgraded the embedded Arti and Tor crate family to `0.45.0`, including the onion-stream API migration required by that release.
- Updated direct dependencies to their latest compatible releases, including `base64 0.23`, `tower-http 0.7`, `infer 0.22`, and `ureq 3.4`.
- Kept `rusqlite` at `0.36` so Cargo resolves a single compatible `libsqlite3-sys` version with Arti `0.45.0`.

### Release Blockers Fixed
- Standardized the release artifact, install docs, CI, and operator commands on the single `rustpost-cli` binary.
- Bumped the crate version to `1.0.0` and refreshed `Cargo.lock`.
- Updated first-run command hints to use the canonical `rustpost-cli` binary.
- Removed the nonfunctional standalone `print-onion-address` command; active onion addresses are reported by the running service.
- Fixed login so existing stored passwords continue to work after an operator raises the configured minimum password length.
- Rejected unsafe profile website URL schemes server-side and stopped rendering unsafe legacy profile website values as links.
- Enforced configured per-post image and video attachment limits and raised the multipart body limit to cover valid configured media mixes.
- Hardened invalid upload cleanup with a cancellation-safe staged-file guard so rejected malformed, oversized, or staging-error uploads do not leave orphaned files under `tmp/uploads`.
- Corrected generated settings copy for username, display name, and bio limits from bytes to characters.

### Release Validation
- Added regression coverage for malformed and oversized rejected upload cleanup, guard-drop cleanup, and successful durable-media handoff.
- Completed a focused invalid-upload cleanup probe and three live disposable stress reruns across public browsing, auth, social, media, admin/diagnostics, and no-JS/Tor-like browser behavior with no pending staged uploads.
- Updated the ignored local Playwright release harness so it no longer configures the rejected `tor.display_onion_address`; synthetic local Tor-header assertions were removed while opt-in real-onion coverage remains.
- Documented the accepted v1.0.0 `cargo audit` exception for transitive Arti `rsa 0.9.10` / `RUSTSEC-2023-0071`, for which no fixed upgrade is currently available.
- Recorded unmaintained transitive Arti dependencies as upstream maintenance caveats rather than RustPost application vulnerabilities.
- Kept Playwright files and generated artifacts ignored and out of the repository.
- Audited the release build, clean temporary-runtime first-run flow, restart persistence, backup/restore behavior, runtime permissions, and operator documentation before tagging.

### Database Lineage
- Squashed the pre-release internal migration chain into a clean first-release SQLite schema baseline at database schema version `2`.
- Fresh databases are now initialized directly from the baseline instead of replaying alpha development migrations.
- Existing current alpha databases that structurally match the baseline are marked as baseline version `2` without destructive changes or data loss.
- Incomplete, unknown, or structurally unsafe pre-release databases now fail closed with administrator guidance to back up/export/recreate/restore instead of attempting blind migration.
- Added stricter schema diagnostics for required tables, columns, indexes, and triggers so startup, backups, restores, and admin health can report incompatible or corrupt database structure clearly.
- `check` now reports database schema status without creating or migrating the database during diagnostics.
- Future post-release database changes should use normal forward migrations after baseline version `2`; released migration history must not be squashed or rewritten after the first public release.

## v0.1.6 - Pinned Profiles

### Profiles and Timelines
- Added profile timeline tabs for posts, replies, media, and liked posts.
- Added pinned profile posts with owner-only pin/unpin controls and automatic cleanup when a pinned post is deleted.
- Added a liked-posts visibility setting so users can make their likes private while still seeing their own likes.
- Preserved relationship and moderation visibility rules across profile tabs, pinned posts, private likes, blocked users, muted users, suspended accounts, and anonymous viewers.

### Posting and Composer
- Added server-enforced post editing during a configurable short edit window, with no-JS edit forms and edited markers on changed posts.
- Added live mention autocomplete to composer text areas backed by bounded `/mentions` suggestions.
- Greyed out composer submit buttons when text exceeds the configured character limit.

### Onboarding and Notifications
- Added first-run account onboarding for new users with profile setup, optional avatar upload, and follow suggestions.
- Grouped notifications by activity target, added grouped read/open handling, and improved empty-state rendering across feeds, lists, search, bookmarks, and notifications.

### Admin, Settings, and Operations
- Added `posts.post_edit_window_seconds` to settings and deep server settings, including validation from `0` to `300` seconds.
- Rebuilt backup archives around deterministic tar output plus `manifest.toml` with RustPost version, DB schema version, component paths, sizes, SHA-256 hashes, timestamps, and Tor-key inclusion state.
- Added full-site backup coverage for SQLite, settings, media variants, thumbnails/transcodes, assets, required runtime directories, and opt-in Tor onion-service keys.
- Added admin backup creation, admin-only archive downloads, restore-from-upload with confirmation, automatic backup settings, safe retention controls, and recent backup history/status.
- Added scheduled automatic backups, disabled by default, with settings.toml/admin configuration and retention that only prunes automatic archives.
- Hardened restore with staged extraction, manifest/hash verification, SQLite integrity and schema checks, hostile path rejection, pre-restore safety backups, rollback on install failure, operation locking, and strict restored Tor-key permissions.

### Privacy, Security, and UI Fixes
- Hardened mention suggestions so wildcard characters cannot broaden searches and unavailable or hidden users stay excluded.
- Hid private likes from profile likes tabs, notifications, and visible like counts except for the liker.
- Rejected non-image onboarding/profile-picture uploads before creating profile avatar state.
- Fixed the no-JS banner so it appears only when JavaScript is disabled.
- Restored the compact Tor onion address pill with a disclosure and copy control.

### Migrations and Compatibility
- Added migrations for onboarding completion state and liked-post visibility.
- Existing active users are marked as already onboarded, deleted users remain incomplete, and existing users keep public likes by default.
- Bumped the crate version to `0.1.6` and refreshed `Cargo.lock`.

## v0.1.5 - Safety, Media, and Link Previews

### Posts and Composer
- Added YouTube link previews for posts so shared video links render with richer inline context.
- Polished the post composer UI and tightened the shared shell sidebar spacing.
- Added a compact quote action icon while preserving the existing posting flow.
- Added a no-JS banner and progressive enhancement fallback behavior for interactive page controls.

### Media Safety and Storage
- Added NSFW media marking, blur-by-default rendering, per-user NSFW blur preferences, and admin controls for marking existing media posts.
- Added a migration and settings support for global NSFW blur defaults.
- Added duplicate media detection with canonical media reuse and cleanup so repeated uploads can share stored variants safely.
- Improved original upload handling so original and transcoded variants share stable basenames.

### Account and Admin Controls
- Added optional registration CAPTCHA support with single-use challenge validation.
- Exposed CAPTCHA and NSFW blur settings through deep admin settings.
- Expanded the admin users investigation panel with richer search and post-context tooling.

### Privacy, Networking, and Performance
- Added gzip compression for browser text responses while leaving media and binary uploads uncompressed.
- Added configurable Tor mirror display in the header and refined the Tor header into a compact status link.
- Kept the embedded Tor client alive through shared ownership for more reliable background onion service operation.

### Release Prep
- Bumped the crate version to `0.1.5` and refreshed `Cargo.lock`.
- Preserved the ignored Playwright artifact policy while removing tracked Playwright test artifacts from the branch.
- Fixed focused Clippy findings in the composer, CAPTCHA, media, and rendering paths.

## v0.1.4 - Layout Foundation

### Core UI Layout
- Rebuilt the shared page shell around a centered three-column layout for desktop with a stable primary reading column.
- Collapsed smaller screens to a single readable column with reachable navigation and no horizontal drift.
- Standardized feed, thread, profile, search, settings, form, and admin surfaces around shared card and column primitives.
- Added objective Playwright layout coverage for Chromium and WebKit across mobile, tablet, and desktop viewports.

### Navigation and Screenshots
- Added matching icons to the main navigation links, including home, following, search, notifications, bookmarks, profile, admin, login, register, and logout.
- Refreshed the README/changelog screenshots from the updated local demo layout.

| Home feed | Profile |
|---|---|
| ![RustPost home feed with centered layout and navigation icons](docs/screenshots/home-feed.png) | ![RustPost profile page with centered layout and navigation icons](docs/screenshots/profile.png) |

| Threaded replies | Media posts |
|---|---|
| ![RustPost post thread using the centered reading column](docs/screenshots/post-thread.png) | ![RustPost media post thread with updated card layout](docs/screenshots/media-posts.png) |

| Mobile layout |
|---|
| ![RustPost mobile feed with compact navigation icons](docs/screenshots/mobile.png) |

## v0.1.3 - Control Room

### Admin Control Room
- Added an admin deep server settings page for changing core site, post, account, and media limits from the web UI.
- Deep settings changes now show a confirmation step with friendly labels, old and new values, and explicit save/discard intents.
- Deep settings writes preserve unrelated settings file values and comments while validating the rewritten configuration before saving.
- Added coverage for valid deep settings updates, invalid submissions, preview/discard flows, and confirmation intent handling.

### Account Settings
- Expanded account settings into a fuller profile, privacy, and account controls area.
- Added persisted dark mode as a per-user setting.
- Added profile location support.
- Added muted users and muted words management, including timeline/search filtering for muted terms.
- Added password change support with current-password validation and confirmation matching.
- Moved blocked users, muted users, muted words, password changes, and delete-account access into the settings surface.

### Account Deletion
- Added a multi-step account deletion flow with CSRF protection, password confirmation, and a short-lived delete intent.
- Account deletion now removes the user profile, posts, sessions, social relationships, notifications, reports, likes, bookmarks, reposts, muted words, and owned media rows.
- Media file cleanup is constrained to known upload directories and rejects unsafe paths before database mutation.
- File cleanup failures after database scrub are logged without leaving account data behind.

### Timeline and UI
- Post timestamps render as non-linked text instead of extra self-links.
- Thread views continue to show timestamps while timeline cards stay compact.
- Account settings, deep settings, danger states, lists, and dark mode received matching layout and style updates.

### Release Prep and CI
- Bumped the crate version to `0.1.3` and refreshed `Cargo.lock`.
- Kept Playwright-related files ignored while preserving the ignored test-artifact policy.
- Fixed focused Clippy findings for stderr output and optional listener handling.
- Reduced upload handling complexity so strict Clippy passes without the old `too_many_lines` expectation.
- Made startup dashboard path assertions portable for Windows CI and cfg-gated Unix-only terminal helpers.

## v0.1.2

### Search
- Search now has a cleaner page layout and clearer result cards.
- It handles posts, usernames, mentions, and hashtags more consistently.
- Bad or unusual search input no longer sends people to an error page.
- Search results can be liked or opened without leaving the search flow.

### Profiles and Media
- Profile pictures now use thumbnails in compact places, which keeps feeds faster and cleaner.
- Full-size profile pages still show the original image.
- The database now tracks thumbnail paths for stored media.

### Admin
- The admin media jobs view is shorter and easier to read.
- It now shows the most useful job status details and recent failures at a glance.
- Admins can upload a custom favicon and reset back to the built-in one.

### Login and Registration
- Login now shows specific errors for missing accounts, wrong passwords, and unavailable accounts.
- Registration now shows a clear message when a username is already taken.

### Threads
- Thread pages no longer show an extra generic thread header.
- The root post no longer links back to itself, which makes navigation less confusing.
