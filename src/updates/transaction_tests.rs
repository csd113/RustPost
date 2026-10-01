use super::*;
use std::cell::Cell;
use std::net::TcpListener;
use std::thread;

struct Fixture {
    _temp: tempfile::TempDir,
    engine: Engine,
    manifest: Manifest,
    archive: Vec<u8>,
}
fn executable(machine: u16) -> Vec<u8> {
    let mut bytes = vec![0; 128];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&2u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&machine.to_le_bytes());
    bytes
}
fn archive(path: &str, binary: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(binary.len()).expect("length"));
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, path, binary)
        .expect("tar entry");
    let tar = builder.into_inner().expect("tar");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&tar).expect("gzip");
    gzip.finish().expect("gzip finish")
}
fn fixture() -> Fixture {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let temp = tempfile::tempdir().expect("fixture");
    let base = temp
        .path()
        .canonicalize()
        .expect("canonical temporary directory");
    let install = base.join("install");
    let state = base.join("state");
    let data = base.join("data");
    for dir in [
        install.join("versions/1.0.0"),
        state.join("backups"),
        data.join("db"),
        data.join("tmp"),
    ] {
        fs::create_dir_all(dir).expect("directories");
    }
    let binary = executable(62);
    fs::write(
        install.join("versions/1.0.0/rustpost-cli"),
        b"old executable",
    )
    .expect("old executable");
    #[cfg(unix)]
    std::os::unix::fs::symlink("versions/1.0.0", install.join("current")).expect("pointer");
    let settings_path = data.join("settings.toml");
    crate::config::write_default_if_missing(&settings_path).expect("settings");
    let public_key = base.join("public-key.hex");
    fs::write(&public_key, "00".repeat(32)).expect("key");
    let conn = Connection::open(data.join("db/rustpost.sqlite3")).expect("db");
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY); INSERT INTO schema_migrations VALUES (4); CREATE TABLE posts (body TEXT); INSERT INTO posts VALUES ('keep every post');").expect("initial schema");
    drop(conn);
    let archive = archive("rustpost-cli", &binary);
    let manifest = Manifest {
        format: 1,
        version: "1.1.0".into(),
        release_id: 100,
        target: "x86_64-unknown-linux-gnu".into(),
        filename: "rustpost-update-x86_64-unknown-linux-gnu.tar.gz".into(),
        size: u64::try_from(archive.len()).expect("length"),
        sha256: release::digest(&archive),
        executable_sha256: release::digest(&binary),
        executable_size: u64::try_from(binary.len()).expect("length"),
        schema: 5,
        minimum_schema: 4,
        minimum_updater: "1.0.0".into(),
    };
    #[cfg(unix)]
    let web_uid = {
        use std::os::unix::fs::MetadataExt as _;
        fs::metadata(&install).expect("metadata").uid() + 1
    };
    #[cfg(not(unix))]
    let web_uid = 123;
    let engine = Engine {
        config: Config {
            install_dir: install,
            state_dir: state,
            data_dir: data,
            settings_path,
            health_port: 8080,
            web_uid,
            public_key,
            retention: 2,
        },
        key: vec![0; 32],
    };
    Fixture {
        _temp: temp,
        engine,
        manifest,
        archive,
    }
}
fn job(engine: &Engine) -> Status {
    let mut status = Status {
        installed: "1.0.0".into(),
        previous_version: Some("1.0.0".into()),
        target_version: Some("1.1.0".into()),
        job: Some(Uuid::new_v4().to_string()),
        administrator: Some(1),
        ..Status::default()
    };
    engine
        .save(&mut status, Phase::Downloading, "test approved release")
        .expect("journal");
    status
}
struct FakeService<'a> {
    engine: &'a Engine,
    running: Cell<bool>,
    broken_health: bool,
    migration_failure: bool,
    start_failure: bool,
    rollback_failure: bool,
}
impl Service for FakeService<'_> {
    fn stop(&self) -> anyhow::Result<()> {
        self.running.set(false);
        Ok(())
    }
    fn start(&self) -> anyhow::Result<()> {
        let version = self.engine.current_version()?;
        if version == "1.1.0" {
            let conn = Connection::open(self.engine.config.data_dir.join("db/rustpost.sqlite3"))?;
            conn.execute_batch("UPDATE schema_migrations SET version=5; CREATE TABLE upgraded (value TEXT); INSERT INTO upgraded VALUES ('migration ran');")?;
            fs::write(
                &self.engine.config.settings_path,
                "simulated new-version config mutation",
            )?;
            if self.migration_failure {
                anyhow::bail!("simulated migration failure");
            }
            if self.start_failure {
                anyhow::bail!("simulated process start failure");
            }
        } else if self.rollback_failure {
            anyhow::bail!("simulated old process failure");
        }
        self.running.set(true);
        Ok(())
    }
    fn health(&self, version: &str, schema: i64) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.running.get() && self.engine.current_version()? == version,
            "fake service is not healthy"
        );
        anyhow::ensure!(
            !(version == "1.1.0" && self.broken_health),
            "simulated health failure"
        );
        anyhow::ensure!(
            verify_database(&self.engine.config.data_dir.join("db/rustpost.sqlite3"))? == schema,
            "schema mismatch"
        );
        Ok(())
    }
}
fn service(engine: &Engine) -> FakeService<'_> {
    FakeService {
        engine,
        running: Cell::new(true),
        broken_health: false,
        migration_failure: false,
        start_failure: false,
        rollback_failure: false,
    }
}

struct HttpFixture {
    url: String,
    worker: Option<thread::JoinHandle<()>>,
}
impl HttpFixture {
    fn new(bytes: Vec<u8>, status: u16, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("local HTTP fixture");
        let url = format!(
            "http://{}/artifact",
            listener.local_addr().expect("address")
        );
        let worker = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("connection");
            let mut request = [0; 4096];
            let _ = stream.read(&mut request);
            thread::sleep(delay);
            let header = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(&bytes);
        });
        Self {
            url,
            worker: Some(worker),
        }
    }
}
impl ArtifactSource for HttpFixture {
    fn fetch(&self, manifest: &Manifest) -> anyhow::Result<Vec<u8>> {
        let client: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(200)))
            .build()
            .into();
        Ok(client
            .get(&self.url)
            .call()?
            .body_mut()
            .with_config()
            .limit(manifest.size + 1)
            .read_to_vec()?)
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.join().expect("HTTP worker");
        }
    }
}

#[test]
fn realistic_success_download_backup_migrate_activate_restart_and_health() {
    let f = fixture();
    let mut status = job(&f.engine);
    let service = service(&f.engine);
    let source = HttpFixture::new(f.archive.clone(), 200, Duration::ZERO);
    f.engine
        .install_with_source(&mut status, &f.manifest, &service, &source)
        .expect("transaction");
    assert_eq!(status.phase, Phase::Succeeded);
    assert_eq!(f.engine.current_version().expect("current"), "1.1.0");
    assert_eq!(
        verify_database(&f.engine.config.data_dir.join("db/rustpost.sqlite3")).expect("schema"),
        5
    );
    let backup = status.backup.expect("mandatory verified backup");
    assert!(backup.verified);
    assert_eq!(backup.previous_schema, 4);
    assert_eq!(backup.reason, "pre_upgrade");
    let directory = f.engine.config.state_dir.join("backups").join(backup.id);
    assert_eq!(
        verify_database(&directory.join("database.sqlite3")).expect("backup schema"),
        4
    );
    assert!(directory.join("metadata.json").is_file());
    assert!(
        f.engine
            .config
            .install_dir
            .join("versions/1.0.0/rustpost-cli")
            .is_file()
    );
    assert_eq!(
        f.engine.status().expect("durable result").phase,
        Phase::Succeeded
    );
}
#[test]
fn realistic_health_failure_restores_old_executable_database_config_and_service() {
    for failure in ["health", "migration", "start"] {
        let f = fixture();
        let original = fs::read(&f.engine.config.settings_path).expect("original settings");
        let mut status = job(&f.engine);
        let mut service = service(&f.engine);
        service.broken_health = failure == "health";
        service.migration_failure = failure == "migration";
        service.start_failure = failure == "start";
        let source = HttpFixture::new(f.archive.clone(), 200, Duration::ZERO);
        f.engine
            .install_with_source(&mut status, &f.manifest, &service, &source)
            .expect("transaction rollback");
        assert_eq!(status.phase, Phase::RolledBack, "{failure}");
        assert_eq!(f.engine.current_version().expect("pointer"), "1.0.0");
        assert_eq!(
            fs::read(f.engine.config.install_dir.join("current/rustpost-cli")).expect("old binary"),
            b"old executable"
        );
        assert_eq!(
            fs::read(&f.engine.config.settings_path).expect("restored configuration"),
            original
        );
        let conn =
            Connection::open(f.engine.config.data_dir.join("db/rustpost.sqlite3")).expect("db");
        let body: String = conn
            .query_row("SELECT body FROM posts", [], |r| r.get(0))
            .expect("original data");
        assert_eq!(body, "keep every post");
        assert_eq!(
            verify_database(&f.engine.config.data_dir.join("db/rustpost.sqlite3"))
                .expect("old schema"),
            4
        );
        service.health("1.0.0", 4).expect("restored service health");
        assert_eq!(
            f.engine.status().expect("saved result").phase,
            Phase::RolledBack
        );
    }
}
#[test]
fn rollback_failure_retains_backup_and_reports_operator_intervention() {
    let f = fixture();
    let mut status = job(&f.engine);
    let mut service = service(&f.engine);
    service.broken_health = true;
    service.rollback_failure = true;
    let source = HttpFixture::new(f.archive.clone(), 200, Duration::ZERO);
    f.engine
        .install_with_source(&mut status, &f.manifest, &service, &source)
        .expect("result recorded");
    assert_eq!(status.phase, Phase::FailedManualIntervention);
    assert!(
        f.engine
            .config
            .state_dir
            .join("backups")
            .join(&status.backup.expect("protected backup").id)
            .exists()
    );
}
#[test]
fn network_error_timeout_and_checksum_failure_do_not_stop_old_service() {
    for (code, delay, bad_hash) in [
        (503, Duration::ZERO, false),
        (200, Duration::from_millis(400), false),
        (200, Duration::ZERO, true),
    ] {
        let mut f = fixture();
        let mut status = job(&f.engine);
        let service = service(&f.engine);
        if bad_hash {
            f.manifest.sha256 = "0".repeat(64);
        }
        let source = HttpFixture::new(f.archive.clone(), code, delay);
        f.engine
            .install_with_source(&mut status, &f.manifest, &service, &source)
            .expect("safe failure");
        assert_eq!(status.phase, Phase::Failed);
        assert!(service.running.get());
        assert_eq!(f.engine.current_version().expect("current"), "1.0.0");
        assert!(status.backup.is_none());
    }
}
#[test]
fn archive_layout_paths_checksum_and_architecture_are_validated() {
    let f = fixture();
    let dest = tempfile::tempdir().expect("stage");
    stage_archive(&f.archive, &f.manifest, dest.path()).expect("valid archive");
    for (path, machine) in [
        ("../rustpost-cli", 62),
        ("other-binary", 62),
        ("rustpost-cli", 183),
    ] {
        let bytes = executable(machine);
        let mut builder = tar::Builder::new(Vec::new());
        let mut header = tar::Header::new_gnu();
        header.set_size(u64::try_from(bytes.len()).expect("length"));
        header.set_mode(0o755);
        // Set the raw hostile path: tar's safe authoring API itself rejects .. .
        header.as_mut_bytes()[..path.len()].copy_from_slice(path.as_bytes());
        header.set_cksum();
        builder
            .append(&header, bytes.as_slice())
            .expect("hostile fixture");
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&builder.into_inner().expect("tar"))
            .expect("gzip");
        let archive = gzip.finish().expect("gzip");
        let mut manifest = f.manifest.clone();
        manifest.size = u64::try_from(archive.len()).expect("length");
        manifest.sha256 = release::digest(&archive);
        manifest.executable_sha256 = release::digest(&bytes);
        let stage = tempfile::tempdir().expect("stage");
        assert!(
            stage_archive(&archive, &manifest, stage.path()).is_err(),
            "{path}/{machine}"
        );
    }
    let mut wrong = f.manifest.clone();
    wrong.sha256 = "0".repeat(64);
    let dest = tempfile::tempdir().expect("stage");
    assert!(stage_archive(&f.archive, &wrong, dest.path()).is_err());
}
#[test]
fn failed_or_unreadable_database_backup_blocks_update() {
    struct NeverDownload;
    impl ArtifactSource for NeverDownload {
        fn fetch(&self, _: &Manifest) -> anyhow::Result<Vec<u8>> {
            panic!("preflight must fail before download");
        }
    }
    let f = fixture();
    let mut status = job(&f.engine);
    let service = service(&f.engine);
    fs::write(
        f.engine.config.data_dir.join("db/rustpost.sqlite3"),
        b"corrupt database",
    )
    .expect("corrupt fixture");
    f.engine
        .install_with_source(&mut status, &f.manifest, &service, &NeverDownload)
        .expect("safe result");
    assert_eq!(status.phase, Phase::Failed);
    assert!(status.backup.is_none());
    assert!(service.running.get());
}
#[test]
fn snapshot_captures_live_wal_without_changing_schema() {
    let f = fixture();
    let conn = Connection::open(f.engine.config.data_dir.join("db/rustpost.sqlite3"))
        .expect("live connection");
    conn.execute_batch("PRAGMA journal_mode=WAL; INSERT INTO posts VALUES ('WAL-only post');")
        .expect("live write");
    let mut status = job(&f.engine);
    f.engine
        .snapshot(&mut status, &f.manifest)
        .expect("snapshot");
    let backup = status.backup.expect("backup");
    let path = f
        .engine
        .config
        .state_dir
        .join("backups")
        .join(backup.id)
        .join("database.sqlite3");
    let conn = Connection::open(path).expect("backup");
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM posts", [], |r| r.get(0))
        .expect("count");
    assert_eq!(count, 2);
    assert_eq!(backup.previous_schema, 4);
}
#[test]
fn interrupted_activation_recovery_restores_before_old_service_start() {
    for phase in [
        Phase::Activating,
        Phase::Restarting,
        Phase::HealthChecking,
        Phase::RollingBack,
        Phase::RestartingPrevious,
    ] {
        let f = fixture();
        let mut status = job(&f.engine);
        let service = service(&f.engine);
        f.engine.snapshot(&mut status, &f.manifest).expect("backup");
        let stage = f.engine.config.install_dir.join("versions/1.1.0");
        fs::create_dir(&stage).expect("staged");
        fs::write(stage.join("rustpost-cli"), executable(62)).expect("staged executable");
        f.engine.activate("1.1.0").expect("activate");
        service.start().expect("migration");
        f.engine
            .save(&mut status, phase, "interrupted")
            .expect("journal");
        f.engine.recover(&service).expect("recover");
        assert_eq!(f.engine.status().expect("status").phase, Phase::RolledBack);
        service.health("1.0.0", 4).expect("old version healthy");
    }
}
#[test]
fn staged_interruption_and_os_locks_recover_without_stale_lock_guessing() {
    let f = fixture();
    let lock = f.engine.lock().expect("first lock");
    assert!(f.engine.lock().is_err());
    drop(lock);
    f.engine.lock().expect("released on close");
    let mut status = job(&f.engine);
    let service = service(&f.engine);
    f.engine
        .save(&mut status, Phase::Staged, "interrupted staging")
        .expect("journal");
    service.stop().expect("stopped");
    f.engine.recover(&service).expect("recovery");
    service.health("1.0.0", 4).expect("old version running");
    assert_eq!(f.engine.status().expect("result").phase, Phase::Failed);
}
#[test]
fn retention_protects_active_rollback_backup_and_rejects_path_escape() {
    let f = fixture();
    let mut status = job(&f.engine);
    for _ in 0..5 {
        status.job = Some(Uuid::new_v4().to_string());
        f.engine
            .snapshot(&mut status, &f.manifest)
            .expect("snapshot");
    }
    let protected = status.backups[4].clone();
    status.backup = Some(protected.clone());
    f.engine.prune(&mut status).expect("retention");
    assert_eq!(status.backups.len(), 3);
    assert!(
        f.engine
            .config
            .state_dir
            .join("backups")
            .join(&protected.id)
            .is_dir()
    );
    status.backups.push(BackupInfo {
        id: "../escape".into(),
        ..protected
    });
    assert!(f.engine.prune(&mut status).is_err());
}
#[test]
fn corrupt_rollback_backup_never_starts_old_binary_against_upgraded_schema() {
    let f = fixture();
    let mut status = job(&f.engine);
    f.engine
        .snapshot(&mut status, &f.manifest)
        .expect("snapshot");
    let path = f
        .engine
        .config
        .state_dir
        .join("backups")
        .join(&status.backup.as_ref().expect("backup").id)
        .join("database.sqlite3");
    fs::write(path, b"corrupt").expect("corrupt fixture");
    let service = service(&f.engine);
    f.engine
        .rollback(&mut status, &service, "test")
        .expect("failure persisted");
    assert_eq!(status.phase, Phase::FailedManualIntervention);
    assert!(!service.running.get());
}

#[test]
fn protected_ancestors_reject_writable_parent_even_for_private_leaf() {
    use std::os::unix::fs::PermissionsExt as _;
    let f = fixture();
    let parent = f.engine.config.install_dir.parent().expect("parent");
    protected_ancestors(&f.engine.config.install_dir, f.engine.config.web_uid).expect("protected");
    fs::set_permissions(parent, fs::Permissions::from_mode(0o777)).expect("writable ancestor");
    assert!(f.engine.validate().is_err());
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700)).expect("restore");
}
#[test]
fn orphan_snapshots_and_failed_attempts_obey_terminal_retention() {
    let f = fixture();
    let mut status = job(&f.engine);
    for _ in 0..6 {
        status.job = Some(Uuid::new_v4().to_string());
        f.engine
            .snapshot(&mut status, &f.manifest)
            .expect("snapshot published without journal");
    }
    // Persisted journal has no snapshot history: simulate death between
    // snapshot rename/fsync and the subsequent phase save.
    f.engine
        .recover(&service(&f.engine))
        .expect("recover orphans");
    let recovered = f.engine.status().expect("history");
    assert_eq!(recovered.phase, Phase::Failed);
    assert_eq!(recovered.backups.len(), 2);
    assert_eq!(
        fs::read_dir(f.engine.config.state_dir.join("backups"))
            .expect("backups")
            .count(),
        2
    );
}
#[test]
fn failed_health_can_retry_identical_signed_staged_program() {
    let f = fixture();
    let mut first = job(&f.engine);
    let mut broken = service(&f.engine);
    broken.broken_health = true;
    f.engine
        .install_with_source(
            &mut first,
            &f.manifest,
            &broken,
            &HttpFixture::new(f.archive.clone(), 200, Duration::ZERO),
        )
        .expect("rollback");
    assert_eq!(first.phase, Phase::RolledBack);
    let mut second = job(&f.engine);
    f.engine
        .install_with_source(
            &mut second,
            &f.manifest,
            &service(&f.engine),
            &HttpFixture::new(f.archive.clone(), 200, Duration::ZERO),
        )
        .expect("retry");
    assert_eq!(second.phase, Phase::Succeeded);
}

#[test]
fn archive_expansion_and_trailing_contents_are_bounded() {
    let f = fixture();
    for padding in [vec![0; 128 * 1024], b"unexpected payload".to_vec()] {
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gzip.write_all(&padding).expect("gzip trailing");
        let mut bytes = f.archive.clone();
        bytes.extend(gzip.finish().expect("gzip"));
        let mut manifest = f.manifest.clone();
        manifest.size = u64::try_from(bytes.len()).expect("size");
        manifest.sha256 = release::digest(&bytes);
        let stage = tempfile::tempdir().expect("stage");
        assert!(stage_archive(&bytes, &manifest, stage.path()).is_err());
    }
}

#[test]
fn archive_rejects_hidden_gnu_metadata_entries() {
    let f = fixture();
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_entry_type(tar::EntryType::GNULongName);
    header.set_size(13);
    header.set_cksum();
    builder
        .append_data(&mut header, "././@LongLink", b"rustpost-cli\0".as_slice())
        .expect("hidden metadata");
    let binary = executable(62);
    let mut header = tar::Header::new_gnu();
    header.set_size(u64::try_from(binary.len()).expect("length"));
    header.set_cksum();
    builder
        .append_data(&mut header, "rustpost-cli", binary.as_slice())
        .expect("binary");
    let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gzip.write_all(&builder.into_inner().expect("tar"))
        .expect("gzip");
    let bytes = gzip.finish().expect("gzip");
    let mut manifest = f.manifest;
    manifest.size = u64::try_from(bytes.len()).expect("size");
    manifest.sha256 = release::digest(&bytes);
    let stage = tempfile::tempdir().expect("stage");
    assert!(stage_archive(&bytes, &manifest, stage.path()).is_err());
}
