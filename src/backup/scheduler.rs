//! Scheduler for the backup subsystem.

use super::*;

pub fn run_automatic_backup_if_due(
    base_paths: &RuntimePaths,
    settings: &Settings,
) -> anyhow::Result<Option<PathBuf>> {
    if !settings.backup.enabled || !settings.backup.automatic_enabled {
        return Ok(None);
    }
    let mut paths = RuntimePaths::from_data_dir(base_paths.data_dir.clone())
        .with_tor_data_dir(&settings.tor.data_dir)
        .with_backup_dir(&settings.backup.backup_dir);
    paths.settings_path.clone_from(&base_paths.settings_path);
    paths.ensure()?;
    if !automatic_backup_due(&paths, &settings.backup)? {
        return Ok(None);
    }
    let _temp_operation = crate::runtime::begin_temp_operation();
    let _lock = OperationLock::acquire(&paths)?;
    if !automatic_backup_due(&paths, &settings.backup)? {
        return Ok(None);
    }
    let archive = create_backup_inner(
        &paths,
        settings.backup.automatic_include_tor_keys,
        BackupKind::Automatic,
        true,
    )?;
    apply_retention(&paths, &settings.backup)?;
    Ok(Some(archive))
}

pub fn spawn_automatic_scheduler(
    base_paths: RuntimePaths,
    settings_path: PathBuf,
    shutdown_rx: watch::Receiver<bool>,
) {
    let _task = spawn_automatic_scheduler_observed(
        base_paths,
        settings_path,
        shutdown_rx,
        crate::console::operations::Operations::default(),
    );
}

pub fn spawn_automatic_scheduler_observed(
    base_paths: RuntimePaths,
    settings_path: PathBuf,
    mut shutdown_rx: watch::Receiver<bool>,
    operations: crate::console::operations::Operations,
) -> tokio::task::JoinHandle<()> {
    use crate::console::operations::TaskState;
    tokio::spawn(async move {
        loop {
            if *shutdown_rx.borrow() {
                break;
            }
            operations.set(
                0,
                TaskState::Running,
                "checking schedule / creating archive if due",
            );
            let check_paths = base_paths.clone();
            let check_settings = settings_path.clone();
            let result = tokio::task::spawn_blocking(move || {
                Settings::load(&check_settings).and_then(|settings| {
                    settings.validate()?;
                    let enabled = settings.backup.enabled && settings.backup.automatic_enabled;
                    run_automatic_backup_if_due(&check_paths, &settings)
                        .map(|archive| (enabled, archive))
                })
            })
            .await;
            match result {
                Ok(Ok((_, Some(path)))) => {
                    operations.set(
                        0,
                        TaskState::Succeeded,
                        "archive created and retention completed",
                    );
                    info!(archive = %path.display(), "automatic backup created");
                }
                Ok(Ok((true, None))) => {
                    operations.set(0, TaskState::Skipped, "no automatic backup due")
                }
                Ok(Ok((false, None))) => {
                    operations.set(0, TaskState::Skipped, "automatic backup disabled")
                }
                Ok(Err(error)) => {
                    operations.set(
                        0,
                        TaskState::Failed,
                        crate::console::operations::failure_reason(&error),
                    );
                    warn!(diagnostic = crate::console::operations::failure_reason(&error), error = %error, "automatic backup check failed");
                }
                Err(error) => {
                    operations.set(0, TaskState::Failed, "backup worker failed");
                    warn!(error = %error, "automatic backup worker failed");
                }
            }
            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() { break; }
                }
                () = tokio::time::sleep(Duration::from_secs(AUTOMATIC_CHECK_INTERVAL_SECS)) => {}
            }
        }
    })
}

pub fn apply_retention(paths: &RuntimePaths, settings: &BackupSettings) -> anyhow::Result<usize> {
    let mut automatic = list_backups(paths)?
        .into_iter()
        .filter(|archive| archive.automatic && archive.manifest_valid)
        .collect::<Vec<_>>();
    automatic.sort_by(|left, right| right.filename.cmp(&left.filename));
    let keep_last = settings.retention_keep_last.max(1);
    let cutoff = retention_cutoff(settings.retention_max_age_days);
    let mut deleted = 0usize;
    for (index, archive) in automatic.iter().enumerate() {
        // Never prune the protected newest set, even when all copies are old.
        if index < keep_last {
            continue;
        }
        let over_count = automatic.len().saturating_sub(deleted) > keep_last;
        let over_age = cutoff.is_some_and(|cutoff| {
            fs::metadata(&archive.path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified < cutoff)
        });
        if over_count || over_age {
            fs::remove_file(&archive.path)
                .with_context(|| format!("failed to remove old backup {}", archive.filename))?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

pub(super) fn automatic_backup_due(
    paths: &RuntimePaths,
    settings: &BackupSettings,
) -> anyhow::Result<bool> {
    let Some(latest) = list_backups(paths)?
        .into_iter()
        .filter(|archive| archive.automatic && archive.manifest_valid)
        .max_by(|left, right| left.filename.cmp(&right.filename))
    else {
        return Ok(true);
    };
    let modified = fs::metadata(latest.path)?.modified()?;
    let interval = Duration::from_secs(settings.automatic_interval_minutes.saturating_mul(60));
    Ok(modified.elapsed().unwrap_or(Duration::ZERO) >= interval)
}

pub(super) fn retention_cutoff(days: u64) -> Option<SystemTime> {
    if days == 0 {
        return None;
    }
    SystemTime::now().checked_sub(Duration::from_secs(days.saturating_mul(86_400)))
}
