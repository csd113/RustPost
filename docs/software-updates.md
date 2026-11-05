# Updating RustPost

Updates are installed by the person running the site. Save a backup, download the new release, stop the app, replace the executable, and start it with the **same data directory**.

[All guides](README.md) · [Backups](operator-guide.md#backup-and-restore) · [Docker upgrades](docker.md#replacing-or-upgrading-the-container)

## Check for a release

In v1.1.0, open **Admin → Software updates** and choose **Check for updates**. RustPost asks GitHub for the latest published stable release. If its version is newer than the running package version, the page shows a link to that release.

The check does not download, install, restart, or replace anything. Drafts and prereleases are excluded. Network failures leave the site running and show that the check could not complete; you can visit [GitHub Releases](https://github.com/csd113/RustPost/releases) directly or try later.

The v1.1.0 build reports version **1.1.0**. The checker compares version numbers, not Git commits, so “up to date” does not mean a development checkout matches the latest dev commit or a published archive. Before v1.1.0 is published, a prepared 1.1.0 build can also report “up to date” against the older published release. Keep the source commit with your build records.

There is no automatic updater service, local updater IPC endpoint, or privileged installation helper in v1.1.0. Older upgrade notes that describe those components are historical.

## Replace a downloaded app

1. Read the target release notes and check platform compatibility. Download its archive and checksum, then [verify the download](getting-started.md#verify-a-download).
2. Create a [full-site backup](operator-guide.md#backup-and-restore) and copy it off the machine. If you need to preserve an onion identity, explicitly include and protect its keys.
3. Stop RustPost and wait for shutdown. Pause any service manager that would restart it.
4. Keep the old executable as a rollback reference. Extract the new executable into the app folder **without deleting or replacing `rustpost-data`**. Use `chmod +x rustpost-cli` on Linux/macOS if necessary.
5. Start it with your original command and data path:

   ```sh
   ./rustpost-cli --data-dir ./rustpost-data serve
   ```

   On Windows, use `.\rustpost-cli.exe --data-dir .\rustpost-data serve`. A service should use its original absolute data path.

6. Log in and verify accounts, posts, media, and administrator access. Keep the backup until you have confirmed the new version works.

Startup may apply forward database migrations. Do not assume replacing the executable with an older one can read an upgraded database. If rollback is needed, stop the app and use a compatible old binary with the pre-upgrade backup, following the [offline restore procedure](operator-guide.md#restore-with-the-server-stopped). Keep newer data separately if it needs recovery.

## Containers and source builds

For Docker, [replace the container](docker.md#replacing-or-upgrading-the-container) using the intended published image and retain the same `/data` volume. Restarting an existing container does not pull a newer image.

For source builds, record the commit, build and validate it using the [development guide](development.md), then follow the same backup/stop/replace/start process. Do not overwrite a running executable or assume a newer commit has a newer package version.

## Release-check details

The admin check is CSRF-protected, permits one in-flight request, and allows five checks per administrator per 15 minutes. It uses GitHub's latest-release API, a 15-second total timeout, a 5-second connection timeout, a 512 KiB response cap, and no HTTP redirects. The displayed link is constructed from a validated stable version tag. The result is held in process memory, so restarting clears it.

A release link is a discovery aid. Verify the downloaded artifact and review the release notes before installation.
