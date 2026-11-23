//! Process interruption must release native ownership without leaving the website paused.

use rustpost::{backup, config::Settings, runtime::RuntimePaths, storage_limits};
use std::{
    fs::File,
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct BackupChild(Child);
impl Drop for BackupChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn fixture() -> anyhow::Result<(tempfile::TempDir, RuntimePaths)> {
    let directory = tempfile::tempdir()?;
    let paths = RuntimePaths::from_data_dir(directory.path().join("data"));
    paths.ensure()?;
    std::fs::write(&paths.settings_path, toml::to_string(&Settings::default())?)?;
    Ok((directory, paths))
}

fn coordination(paths: &RuntimePaths) -> anyhow::Result<File> {
    Ok(File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.tmp_dir.join("backup-coordination.lock"))?)
}

#[test]
fn killed_backup_capture_recovers_without_clearing_a_live_owner() -> anyhow::Result<()> {
    let (_directory, paths) = fixture()?;
    let snapshot = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(paths.data_dir.join("snapshot-coordination.lock"))?;
    snapshot.try_lock_shared()?;
    let mut child = BackupChild(
        Command::new(env!("CARGO_BIN_EXE_rustpost-cli"))
            .arg("--data-dir")
            .arg(&paths.data_dir)
            .args(["--console", "off", "backup"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let marker = paths.tmp_dir.join("backup-capture.lock");
    let deadline = Instant::now() + Duration::from_secs(15);
    while !marker.is_dir() && Instant::now() < deadline {
        assert!(
            child.0.try_wait()?.is_none(),
            "backup exited before capture"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(marker.is_dir(), "backup did not enter capture");
    let coordination = coordination(&paths)?;
    assert!(
        coordination.try_lock().is_err(),
        "child must hold exclusive backup ownership"
    );
    assert!(backup::operation_in_progress(&paths));
    assert!(marker.is_dir(), "a live capture cannot be cleared");
    let contender = Command::new(env!("CARGO_BIN_EXE_rustpost-cli"))
        .arg("--data-dir")
        .arg(&paths.data_dir)
        .args(["--console", "off", "backup"])
        .stdin(Stdio::null())
        .output()?;
    assert!(
        !contender.status.success(),
        "a second backup cannot acquire live ownership"
    );
    assert!(marker.is_dir());
    child.0.kill()?;
    child.0.wait()?;
    // OS ownership is now gone, but abrupt termination skipped marker cleanup.
    assert!(marker.is_dir());
    assert!(!backup::operation_in_progress(&paths));
    assert!(!marker.exists());
    coordination.try_lock()?;
    snapshot.unlock()?;
    Ok(())
}

#[test]
fn stale_marker_recovery_is_fail_closed_and_keeps_unexpected_contents() -> anyhow::Result<()> {
    let (_directory, paths) = fixture()?;
    let marker = paths.tmp_dir.join("backup-capture.lock");
    std::fs::create_dir(&marker)?;
    let ownership = coordination(&paths)?;
    ownership.try_lock()?;
    assert!(backup::operation_in_progress(&paths));
    assert!(marker.is_dir());
    ownership.unlock()?;
    std::fs::write(marker.join("unexpected"), b"preserve evidence")?;
    assert!(backup::operation_in_progress(&paths));
    assert_eq!(
        std::fs::read(marker.join("unexpected"))?,
        b"preserve evidence"
    );
    std::fs::remove_file(marker.join("unexpected"))?;
    assert!(!backup::operation_in_progress(&paths));
    assert!(!marker.exists());
    Ok(())
}

#[test]
fn concurrent_headroom_checks_are_admission_probes_not_disk_reservations() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let available = storage_limits::available_space(directory.path())?;
    anyhow::ensure!(available > 4096, "test filesystem is full");
    let additional = available / 4 * 3;
    let barrier = std::sync::Barrier::new(2);
    let probe = |path: &Path| {
        barrier.wait();
        storage_limits::ensure_headroom(path, 1, additional)
    };
    let (one, two) = std::thread::scope(|scope| {
        let first = scope.spawn(|| probe(directory.path()));
        let second = scope.spawn(|| probe(directory.path()));
        (first.join(), second.join())
    });
    one.map_err(|_| anyhow::anyhow!("first probe panicked"))??;
    two.map_err(|_| anyhow::anyhow!("second probe panicked"))??;
    assert!(additional.saturating_mul(2) > available);
    assert_eq!(std::fs::read_dir(directory.path())?.count(), 0);
    Ok(())
}
