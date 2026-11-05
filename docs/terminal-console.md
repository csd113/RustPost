# Terminal console

The terminal window lets you see how your running RustPost site is doing. It stays open while the app serves the website; use a browser for posting and administration.

[All guides](README.md) · [First run](getting-started.md) · [Operator guide](operator-guide.md)

This guide describes the live console in v1.1.0. The published v1.0.0 build has an older display.

## Everyday controls

| Key | Action |
| --- | --- |
| 1–6 or Tab / Shift+Tab | Switch between Overview, Tasks, Content, Logs, System, and Configuration |
| Arrows or J/K | Scroll the current view |
| Page Up / Page Down, Home / End | Move through longer views |
| R | Request a status refresh |
| ? or H | Open or close help |
| Q | Ask to stop the server; confirm to shut down |
| Ctrl+C | Request graceful shutdown directly |

The website becomes unavailable when the process stops. Wait for shutdown to finish before starting it again or restoring data.

## Choose the display

From the executable's folder:

```sh
./rustpost-cli --data-dir ./rustpost-data --console plain serve
```

Use `.\rustpost-cli.exe` on Windows. `auto` chooses a full-screen display when the terminal supports it; `plain` prints line reports; `off` disables the console and its samplers. `tui` requests the full-screen view but can still fall back when terminal setup fails. Add `--ascii` when your terminal cannot display account names or other Unicode text correctly.

## Understand status before acting

**STALE** means RustPost is showing its last successful measurement with an age and reason. **UNAVAILABLE** means there is no usable value. Neither means the corresponding count is zero.

An onion address or descriptor status does not prove that someone else can connect through Tor. Test it from a separate Tor client. Log summaries are bounded operational information, not a complete audit record.

## Sampling, logging, and terminal details

`serve` selects the full-screen console when stdin/stdout are terminals and `TERM` is not `dumb`. Use `--console auto|tui|plain|off`; terminal initialization or capability failure falls back to plain reports. `off` disables the console and its samplers. Command results stay on stdout; operational reports/logs use stderr. Redirected/dumb output contains no ANSI. The console uses monochrome text and ASCII borders; `--ascii` displays Unicode data as explicit code points.

Every content/help view scrolls, including below 44×14. Raw mode and the alternate screen restore when stopping begins, while plain stopping reports can continue.

Health and freshness are independent. Counts update every 3 seconds through a separate read-only SQLite connection with a 250 ms query budget; forced refreshes coalesce and run at most once/second. DB main/WAL/SHM file lengths update separately every 3 seconds. Upload file lengths are scanned every 30 seconds without following symlinks, with a 10,000-entry/200 ms cooperative budget. Incomplete/failed collections preserve last-good values as **STALE** with age/reason; missing values are **UNAVAILABLE**. File lengths are not allocated disk blocks. OS filesystem calls themselves have no hard deadline.

Listeners display actual bound sockets, including ephemeral ports. Tor distinguishes address assignment, Arti introduction-point/descriptor status, forwarding errors, and the age of observed traffic; current external reachability remains unverified. Task failures and last failure age are retained for the process lifetime independently of log filtering. HTTP counters count handlers and 5xx responses; RSS/CPU and prior-process task outcomes are explicitly unavailable.

Logs retain the last 200 safe event summaries and write `logs/events.log` (1 MiB plus one previous file). Only allowlisted messages/diagnostic categories and source metadata are retained; arbitrary diagnostic fields, request data and secrets are omitted. The Logs view identifies queue drops, evictions and persistence failures. Bounded nonblocking queues isolate logging IO; event-sink drain waits at most one second at exit. This is best-effort event retention, not a durable audit journal.

Shutdown owns the scheduler tasks and finishes admitted backup/maintenance work. HTTP drain is capped at 30 seconds; transactional backup work is not force-aborted. There is no hard whole-process bound for a stuck filesystem. Native Windows console/VT/code-page behavior and Linux SSH/tmux behavior require native interactive validation; portable builds alone do not establish it.
