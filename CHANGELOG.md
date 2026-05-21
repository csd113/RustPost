# Changelog

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
