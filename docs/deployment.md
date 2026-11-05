# Make your site public

The [quick start](getting-started.md) runs RustPost for your own computer. To invite other people, run it on a computer or server that stays on and give visitors an address they can reach.

The usual public setup is **browser → HTTPS reverse proxy → RustPost**. A reverse proxy accepts encrypted web traffic and forwards it to the app. RustPost itself serves HTTP.

[All guides](README.md) · [Docker](docker.md) · [Settings](operator-guide.md#configuration) · [Backups](operator-guide.md#backup-and-restore)

## Before inviting people

1. Complete local setup and create the administrator.
2. Decide whether anyone can register. Change `accounts.registration_enabled` if you want signups closed.
3. Configure your domain, HTTPS certificate, and reverse proxy.
4. Keep RustPost's HTTP port accessible only to the proxy. Use firewall rules and suitable listener settings for your deployment.
5. Make a backup and arrange an off-host copy.
6. Verify login, posting, media, and restart persistence through the public address.

Use a dedicated unprivileged OS account and a private data directory. For an unattended process, configure your service manager to start the executable with an absolute `--data-dir` and `--console off`, and to send a graceful stop signal. Allow backup work to finish before forcing termination. There is no bundled service-manager installation or updater daemon.

## HTTPS and proxy settings

For a proxy running directly on the same host, edit the existing settings to include:

```toml
[server]
host = "127.0.0.1"
port = 8080
public_url = "https://social.example.com"
cookie_secure = true
trusted_proxy_cidrs = ["127.0.0.1/32", "::1/128"]
```

Replace the example domain with your real HTTPS address; keep the rest of the generated settings file. Validate and restart RustPost. `public_url` is display information, not a substitute for DNS, TLS, host validation, or proxy configuration; generated site links remain relative.

The proxy must preserve the public **Host** header, supply a correct **X-Forwarded-Proto**, and build a trustworthy **X-Forwarded-For** chain. Strip or overwrite untrusted client-supplied forwarding headers at your public edge. Trust only the actual proxy addresses; avoid broad CIDRs that include visitors.

On current dev, RustPost accepts forwarding metadata only from a direct peer listed in `trusted_proxy_cidrs`. It walks X-Forwarded-For from the right through trusted addresses to find the client IP. Untrusted peers use their direct address. Malformed or duplicate forwarding metadata can be rejected. This differs from older documentation describing the setting as unused.

The request guard checks browser origin information and secure-cookie behavior against the request host and scheme. A wrong Host or scheme can cause rejected forms or login failures. Authenticated forms also require CSRF tokens. Test the whole login/posting flow through HTTPS, not just the home page.

### Containers and proxies on other hosts

The Docker image listens on `0.0.0.0` **inside** the container on first boot, but the quick-start port mapping publishes only to the host's `127.0.0.1`. A proxy in a different container or on another host needs an intentionally configured private network path.

Set `trusted_proxy_cidrs` to the actual proxy peer address seen by RustPost. Docker networking may mean that address is not loopback. Keep access to the app port restricted even when its listener is `0.0.0.0`; that setting alone does not secure it. See [Docker storage and settings](docker.md).

## Tor alongside HTTPS

You can add an [onion address](operator-guide.md#tor--arti) while keeping HTTPS available. Current dev distinguishes Arti-forwarded requests from ordinary proxy traffic, so onion clients cannot impersonate a trusted loopback proxy. Onion HTTP does not inherit the clearnet secure-cookie setting.

Test ordinary HTTPS and onion login separately. Publishing an onion address is not proof of external reachability or an anonymity guarantee.

## Health and logs

Use the [terminal console](terminal-console.md), **Admin → Health**, and **Media jobs** to inspect the running app. `/`, `/home`, `/login`, and `/register` are public routes useful for HTTP checks. RustPost does not provide `/healthz` or `/readyz`; the health page requires an admin session.

Operational events are retained in a bounded in-memory log and `logs/events.log`. These omit arbitrary request/secret fields and are best-effort diagnostics, not a durable audit journal. Retain the data directory and back up the whole site using the app's backup format rather than copying a live SQLite file alone.

## Security and dependency review

RustPost hashes passwords with Argon2id, uses HttpOnly/SameSite=Lax session cookies, validates authenticated form tokens, escapes rendered user content, and validates uploaded content rather than trusting filenames. SVG is not an allowed default upload type. These controls still depend on correct deployment and maintained dependencies.

Dependency review is separate from successful compilation and tests. The current development changelog records open upstream **RUSTSEC-2023-0071** and **RUSTSEC-2026-0387** findings. Do not treat an old advisory list as a fresh audit: inspect the latest [Dependency Audit workflow](https://github.com/csd113/RustPost/actions/workflows/audit.yml) report and run `cargo audit --file Cargo.lock` when evaluating a source build or release. The workflow is report-only; a successful workflow status can still include vulnerability findings.

For historical context, v1.0.0 recorded the transitive `rsa 0.9.10` / RUSTSEC-2023-0071 exception and maintenance warnings for `bincode 2.0.1` and `paste 1.0.15`. Those notes are evidence about that release, not a claim that the current dependency graph is clean or identical. Keep the [changelog](../CHANGELOG.md) and the current audit report with your deployment review.

## Practical limits

RustPost serves one independent community and does not federate. Search uses SQLite FTS5 with basic user matching and fixed result limits. Reporting and moderation workflows are limited, and broader accessibility review is still outstanding. Media conversion runs during uploads, so large files and slow hardware can delay requests.

After updates or configuration changes, recheck your public login, posting, media, backup, and restart flows. See [Software updates](software-updates.md) for manual installation and rollback considerations.
