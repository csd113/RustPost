# Getting started

RustPost runs a small social website from one application. This guide takes you from a download to your first post on your own computer. You can make the site public later.

[All guides](README.md) · [Docker instead](docker.md) · [Build from source](development.md#build-and-run)

## Download and extract

1. Open the [latest release](https://github.com/csd113/RustPost/releases/latest) and expand **Assets**.
2. Choose the application archive for your operating system and processor from the [download table](../README.md#1-download-the-app). A recent Mac's **About This Mac** window says whether it uses an Apple chip or Intel. On Linux, `uname -m` commonly prints `x86_64` or `aarch64`.
3. Extract the archive into a folder you can write to. Open the extracted folder until you see `rustpost-cli` (Linux/macOS) or `rustpost-cli.exe` (Windows).
4. Open a terminal there. On Windows, use **Open in Terminal** from that folder and select PowerShell. In a terminal, `cd` changes folders; quote paths that contain spaces.

The v1.2.0 ZIPs contain a folder named for their version and platform, such as `rustpost-cli-v1.2.0-macos-apple-silicon`. Use them when v1.2.0 is published; older v1.0.0 archives contain a `rustpost` folder and use different archive/checksum names. Always work beside the executable. See [Release packaging](development.md#release-packaging) for the older names.

## Start the app

Linux or macOS:

```sh
chmod +x rustpost-cli
./rustpost-cli --data-dir ./rustpost-data serve
```

Windows PowerShell:

```powershell
.\rustpost-cli.exe --data-dir .\rustpost-data serve
```

The first start creates a `rustpost-data` folder beside the app when you run these commands from the executable's folder. It contains settings, accounts, posts, and uploads. Keep it private and retain it when updating.

## Create your administrator

In an interactive terminal, first startup asks for an administrator when none exists. Enter your username, an optional display name if offered, and a unique password. The default minimum password length is 10 characters. Hidden prompts show no password characters; enter it again when asked to confirm.

There is no default administrator password. If the process starts without prompting, use a second terminal in the same folder:

```sh
./rustpost-cli --data-dir ./rustpost-data create-admin-interactive
```

On Windows, use `.\rustpost-cli.exe` in place of `./rustpost-cli`. This command is also how you create the first admin for a background process. Do not use a different data directory: that would create an account for a different site.

## Open and use your site

Wait for startup to finish, then visit **[http://127.0.0.1:8080](http://127.0.0.1:8080)** in a browser on the computer running RustPost.

1. Log in with the administrator account you just created.
2. Complete any profile setup shown after login.
3. Write a short post in the Home Feed. Open the post to reply or view its conversation.
4. Open **Settings** for your profile or **Admin** to manage your site.

`127.0.0.1` means this computer. It is not an address other people can use to visit your server. Follow [Make your site public](deployment.md) when you are ready to share it.

## Stop and start again

Keep the app running while you browse. To stop it, press **Ctrl+C** in its terminal and wait for shutdown to finish. Start it again with the same command and data directory. Your accounts and content remain saved.

Version 1.2.0 has a [live console](terminal-console.md): press **1–6** to switch views, **?** for help, or **Q** to confirm shutdown. Published older builds may have a different terminal display.

## Optional media conversion

You can post allowed images and videos without installing extra tools. Installing `ffmpeg` enables image/video conversion and video thumbnails. If conversion is unavailable or fails, RustPost can retain an allowed upload in its original format; browser playback then depends on that format.

**Admin → Health** reports media-tool availability. The [Docker image](docker.md) includes `ffmpeg`; other installations can provide it on the app's executable search path or set `media.ffmpeg_path` in `settings.toml`. See [Media](operator-guide.md#media-and-ffmpeg) for formats and limits.

## Verify a download

A checksum helps detect a damaged or mismatched download. Get `SHA256SUMS.txt` from the same v1.2.0 GitHub release as the ZIP and check it before extracting. These examples assume the archive and manifest are in your terminal's current folder; substitute your platform's filename from the [download table](../README.md#1-download-the-app).

Linux (one downloaded ZIP):

```sh
sha256sum rustpost-cli-v1.2.0-linux-x86_64.zip
cat SHA256SUMS.txt
```

macOS (Apple silicon; Intel uses `macos-intel`):

```sh
shasum -a 256 rustpost-cli-v1.2.0-macos-apple-silicon.zip
cat SHA256SUMS.txt
```

Windows PowerShell:

```powershell
Get-FileHash .\rustpost-cli-v1.2.0-windows-x86_64.zip -Algorithm SHA256
Get-Content .\SHA256SUMS.txt
```

Compare the complete hash with the manifest line for your exact ZIP filename, ignoring letter case. Do not run an archive whose hash differs; download it and the manifest again from the release page.

With all five ZIPs downloaded, `sha256sum -c SHA256SUMS.txt` (Linux) or `shasum -a 256 -c SHA256SUMS.txt` (macOS) checks the full set and should report `OK` for each file. With only one ZIP, these full-set commands report missing files for the others. Older v1.0.0 downloads use individual `.sha256` files instead; follow the checksum names on that release.

## Troubleshooting

| What happened? | What to do |
| --- | --- |
| The browser says it cannot connect | Check that the app is still running, startup has finished, and the URL is `http://127.0.0.1:8080`. Use the same computer. Read the terminal error if the app exited. |
| “Address already in use” | Another process is using port 8080. Stop the other RustPost process, or edit `port` under `[server]` in `rustpost-data/settings.toml`, restart, and use the new port in the URL. |
| “No such file” or command not found | Open a terminal in the folder containing the executable. Include `./` on Linux/macOS or `.\` in PowerShell. |
| “Permission denied” on Linux/macOS | Run `chmod +x rustpost-cli`; also check that the app can write to its data folder. |
| The OS blocks the download | Confirm its source and checksum. Use the OS's security review for this specific app if you trust it; do not disable system-wide security. |
| Wrong architecture or missing system library | Recheck the download's operating system and processor. Linux releases use GNU/Linux targets; a minimal or older distribution may need a compatible runtime. Docker is an alternative. |
| My site looks empty after restarting | Check the `--data-dir` path and current folder. A different path opens a different site. Stop and restart using the original folder. |
| I forgot the administrator password | Use the [password reset command](operator-guide.md#administrator-accounts) against the original data directory. |
| Settings changed but the site did not | Most settings need a restart. Follow the [configuration guide](operator-guide.md#configuration). |
| Uploads fail or videos do not play | Check upload limits and **Admin → Health / Media jobs**. The original format may not be playable in every browser. |

For reproducible bugs, use the [issue tracker](https://github.com/csd113/RustPost/issues). Include your OS, downloaded release or source commit, command (without secrets), and the error. Do not attach a private database, passwords, account archives, or Tor keys.
