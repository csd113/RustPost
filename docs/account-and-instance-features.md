# Using RustPost

RustPost is a community website for posts, media, and conversations. Log in to write posts and follow people; use **Settings** to manage your own account. Administrators also see **Admin** for site-wide controls.

[All guides](README.md) · [Install RustPost](getting-started.md) · [Detailed account rules](account-reference.md)

## Write and read posts

Write in the Home Feed's post box. The default limit is 280 characters, and the site owner can change it. Attach images or videos if you want, then publish. Open a post to read the conversation and reply.

Use **Following** to read people you follow. Profiles have tabs for posts, replies, media, and liked posts. You can like, repost, quote, or bookmark a post; bookmarks are private. Your profile settings control whether your liked posts are public.

You can edit your own post text for 15 seconds after publishing by default. The site owner can shorten, extend (up to five minutes), or disable that window. Edited posts are marked. You can also pin a post to your profile.

Search finds posts and accounts; notifications collect activity involving you. Core forms work without JavaScript, though enhancements such as mention suggestions require it.

See [community tools](community-tools.md) for drafts, UTC scheduling, per-image descriptions, polls, search filters, history navigation, media processing, and short deletion undo.

## Set up your profile

Open **Settings** to change your display name, bio, location, website, picture, banner, theme, and media-blur preferences. A username is your account handle; a display name is the name shown beside your posts.

To change your username, use the **Username** panel and confirm your current password. Your posts, follows, and account identity stay with you. Previous usernames remain reserved while the account exists, and old profile URLs show history rather than silently becoming another person's profile. See [Username history](account-reference.md#username-changes-and-history-34).

## Choose who you follow and what you see

Visit a profile to follow it. If that account requires approval, your request remains pending until its owner accepts it.

To require approval for your own new followers, enable **Require approval for new followers** in Settings. Review requests on **Follow requests**. Existing followers stay approved. Turning the preference off does not automatically approve outstanding requests; you can still accept or reject them. This controls following; it is not a promise that your posts are private or hidden from public visitors.

| Control | What it does |
| --- | --- |
| Block | Removes follows in both directions and prevents interactions between the accounts. Their posts are hidden from one another's normal views. Unblocking allows interaction again but does not recreate removed follows. |
| Mute | Hides an account's posts from your feeds, search, suggestions, notifications, and profile activity without notifying them. |
| Muted words | Hides matching text in relevant feeds and previews. Matching ignores case and can match inside a word: `cat` also matches `concatenate`. Your own posts remain visible to you. |

These preferences do not erase content from the database. An account being deleted or a post being removed has separate rules.

## Export or import your account

Use the account export/import controls in **Settings**. An export is a `.tar.gz` archive containing your profile information, posts, media, outgoing follows, and muted words. Keep it private: it contains your personal content.

To import, first create or log in to the destination RustPost account, then open its import page and select your archive. Imports add content to that account; they do not create accounts or change the destination username. Non-empty destination profile fields are kept, and the result reports skipped items. Importing the same archive into the same account again is rejected.

Follows can only reconnect to accounts that exist on the destination site. Protected accounts still require approval. Passwords, sessions, administrator access, notifications, likes, bookmarks, and reposts are not transferred. References to other people's posts may not survive a transfer. Drafts, schedules, poll definitions/votes, and deletion recovery are not part of account archive format 1; full-site backups preserve them.

The default compressed archive limit is 300 MiB; the owner can change it. See the [archive format and import rules](account-reference.md#account-export-and-import-29) for the full inclusion list, independent expansion limits, and validation behavior.

An account export is **not a full-site backup**. Administrators should use [Backups](operator-guide.md#backup-and-restore) to preserve the whole community.

## Change your password or delete your account

Change your password in **Settings** using your current password and a new one. An administrator can require you to change it at your next authenticated visit, or sign all of your sessions out. A forced logout does not itself prevent you from logging in again.

To delete your account, use the deletion flow in Settings and confirm your password. By default, you have **30 days to cancel**. The page shows the deadline; the owner can configure another grace period, including immediate deletion.

While deletion is pending, you can log in, read, export your data, change your password, log out, or cancel deletion. Publishing and social actions are restricted. After final deletion, the account's content and relationships are removed; shared media still referenced by another account is retained. Old usernames can be registered again, with a notice identifying the new holder as a different account. See [Deletion rules](account-reference.md#account-deletion-grace-period-33).

## Community announcements and maintenance

Administrators can publish an announcement from **Admin**. It appears near the site name and can be cleared or disabled there.

Maintenance mode pauses registration and ordinary users' publishing while the site remains readable. Logins, likes, bookmarks, follows, settings, exports, and account deletion remain available. Administrators can still publish to verify the site. Maintenance mode is not a shutdown and does not make the database safe to replace: [stop the server before restoring](operator-guide.md#backup-and-restore).

Administrators can find day-to-day controls in the [operator guide](operator-guide.md). Developers and operators needing precise restrictions can read the [account behavior reference](account-reference.md).
