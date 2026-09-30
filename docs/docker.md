# Running RustPost with Docker

Docker runs RustPost in a container: a packaged copy of the app and the tools it needs. The released image is `ghcr.io/csd113/rustpost:1.0.0`, with Linux amd64 and arm64 support and `ffmpeg` included for media conversion.

Start with the [three-step setup in the README](../README.md#run-the-container). This guide covers the next steps. The commands assume your container is named `rustpost` and uses the `rustpost-data` volume from that setup.

## Where your data lives

RustPost saves its settings, database, uploads, backups, logs, and Tor state under `/data` inside the container. The Docker volume stores those files separately from the container itself.

You can replace or remove the container without deleting the volume. **Do not delete the `rustpost-data` volume unless you intend to erase the site.** Keep backups outside the Docker host too.

The container runs as a non-root user with UID and GID `10001`.

### Using an existing folder instead

A bind mount stores data in a folder you choose on the host. On Linux, create a dedicated folder with permissions for the container user before starting the container:

```sh
sudo mkdir -p /srv/rustpost
sudo chown 10001:10001 /srv/rustpost
sudo chmod 700 /srv/rustpost
```

In the README's `docker run` command, replace `-v rustpost-data:/data` with `-v /srv/rustpost:/data`. Use a named volume if you do not need a specific host folder.

For a new data directory, the container creates `settings.toml` and sets `server.host` to `0.0.0.0` so Docker can forward traffic. Existing settings are preserved. If you bring an existing RustPost data directory, edit its `[server]` section to use `host = "0.0.0.0"` before starting it in Docker.

## Editing settings

Copy the settings file to your computer, edit it with a text editor, and copy it back:

```sh
docker cp rustpost:/data/settings.toml ./rustpost-settings.toml
# Edit rustpost-settings.toml before continuing.
docker cp ./rustpost-settings.toml rustpost:/data/settings.toml
docker exec rustpost rustpost-cli --data-dir /data check
docker restart rustpost
```

Only restart if `check` succeeds. If it reports an error, fix the file and copy it back first. Keep the file private: it describes your deployment. The [operator guide](operator-guide.md#configuration) explains the available settings.

The quick start publishes port 8080 on `127.0.0.1`, so only the host computer can connect directly. For a public site, configure HTTPS through a reverse proxy and the corresponding settings described in the [README](../README.md#making-your-site-public).

## Backing up and restoring

Use **Admin → Backups** in the browser to create and download a full-site backup. To create one from the terminal:

```sh
docker exec rustpost rustpost-cli --data-dir /data backup
```

The command prints the archive path under `/data/backups`. To save an archive on your computer, replace `ARCHIVE_NAME.tar` below with that filename:

```sh
docker cp rustpost:/data/backups/ARCHIVE_NAME.tar ./ARCHIVE_NAME.tar
```

Tor private keys are excluded by default. Include them only when needed, and protect any archive containing them.

For a browser restore, use **Admin → Backups**, follow the restore confirmation, then run `docker restart rustpost` so the app loads the restored database and settings.

For a terminal restore, stop the server and use a temporary container with the same volume. First copy the backup into the volume while the original container still exists:

```sh
docker cp ./ARCHIVE_NAME.tar rustpost:/data/backups/restore-input.tar
docker exec --user 0 rustpost chown 10001:10001 /data/backups/restore-input.tar
docker exec --user 0 rustpost chmod 600 /data/backups/restore-input.tar
docker stop rustpost
docker run --rm \
  -v rustpost-data:/data \
  ghcr.io/csd113/rustpost:1.0.0 \
  restore /data/backups/restore-input.tar
```

The ownership commands let the app's non-root user read the copied archive while keeping it private. If restore succeeds, run `docker start rustpost`. If it fails, inspect the reported error before continuing. This example uses a named volume; if you use a bind mount, supply that same mount to the temporary container. See the [restore reference](operator-guide.md#backup-and-restore) for validation, rollback, and Tor-key options.

## Replacing or upgrading the container

Create and download a backup first, and read the target release notes. The example below recreates the container using v1.0.0; substitute the published version you intend to install when upgrading.

Download the image before stopping the app:

```sh
docker pull ghcr.io/csd113/rustpost:1.0.0
docker stop rustpost
docker rm rustpost
docker run -d --name rustpost \
  -p 127.0.0.1:8080:8080 \
  -v rustpost-data:/data \
  --restart unless-stopped \
  ghcr.io/csd113/rustpost:1.0.0
docker logs rustpost
```

Reuse the same volume or bind mount and any custom port or network settings from your original container. You do not need to create the administrator account again. Open the site and confirm your accounts, posts, and media are still there.

The `latest` tag currently points to v1.0.0. A version tag makes the release you are running explicit; restarting an existing container does not download a newer image.

## Troubleshooting

| Symptom | What to check |
| --- | --- |
| The browser cannot reach the site | Run `docker ps -a` and `docker logs rustpost`. Confirm port 8080 is available and you are browsing on the Docker host. |
| An existing data directory will not serve through Docker | Check that `server.host` is `0.0.0.0` inside the container's settings. |
| Permission errors under `/data` | For bind mounts, confirm the dedicated folder is writable by UID/GID `10001`. |
| You cannot log in as administrator | Run the interactive administrator creation command from the README if no administrator exists. See the [CLI reference](operator-guide.md#cli-reference) for password reset commands. |
| Settings changes have no effect | Validate with `rustpost-cli --data-dir /data check` inside the container, then restart it. |
