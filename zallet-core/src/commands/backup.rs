use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use abscissa_core::Runnable;
use rusqlite::{
    Connection, OpenFlags,
    backup::{Backup, StepResult},
};

use crate::{
    cli::BackupCmd,
    commands::AsyncRunnable,
    error::{Error, ErrorKind},
    fl,
    prelude::*,
};

/// How many database pages are copied per backup step.
///
/// The datadir lock means no other Zallet process is writing, so steps exist only
/// to bound memory use and give SQLite a chance to report busy states, not to
/// yield to a concurrent writer.
const PAGES_PER_STEP: std::ffi::c_int = 4096;

/// How many busy/locked steps are tolerated before the backup fails loudly.
///
/// The source is quiesced under the datadir lock, so a persistent busy state means
/// something unexpected holds the database (for example a stray process on a shared
/// datadir). Failing after a bounded number of retries rather than spinning forever
/// is the operator signal #195 asks for: a backup that is not completing must say so.
const MAX_BUSY_STEPS: u32 = 20;

/// How long to wait after a busy/locked step before retrying.
const BUSY_RETRY_PAUSE: Duration = Duration::from_millis(250);

impl AsyncRunnable for BackupCmd {
    async fn run(&self) -> Result<(), Error> {
        let config = APP.config();

        // Backing up requires a quiesced wallet: the Online Backup API can copy
        // around a live writer, but a *consistent restore point* that includes the
        // SQLite sidecar state is only guaranteed when nothing is writing. Holding
        // the datadir lock also means a running `zallet start` and a backup cannot
        // race; periodic backups of a running wallet belong inside the long-running
        // process itself (the second half of #195) rather than in this command.
        let _lock = config.lock_datadir()?;

        let source = config.wallet_db_path();

        // A zero-length file is not yet a SQLite database (SQLite initializes it on
        // first write), so treat it like a missing one, mirroring `Database::open`.
        match fs::metadata(&source) {
            Ok(meta) if meta.len() > 0 => {}
            Ok(_) | Err(_) => {
                return Err(ErrorKind::Generic
                    .context(fl!(
                        "err-backup-no-wallet",
                        path = source.display().to_string(),
                    ))
                    .into());
            }
        }

        let destination = resolve_destination(&self.destination)?;

        let pages = {
            let destination = destination.clone();
            tokio::task::spawn_blocking(move || copy_database(&source, &destination))
                .await
                .map_err(|e| ErrorKind::Generic.context(e))??
        };

        println!(
            "{}",
            fl!(
                "cmd-backup-complete",
                path = destination.display().to_string(),
                pages = pages.to_string(),
            )
        );

        // The wallet database holds key material in encrypted form only; a restore
        // additionally needs the (static) age encryption identity, which this
        // command deliberately does not copy into the same location as the backup —
        // co-locating them would make the backup self-decrypting.
        #[cfg(zallet_build = "wallet")]
        println!(
            "{}",
            fl!(
                "cmd-backup-identity-reminder",
                identity = config.encryption_identity().display().to_string(),
            )
        );

        Ok(())
    }
}

/// Resolves the operator-supplied destination to the backup file's final path.
///
/// An existing directory gets a timestamped file name inside it, so repeated backups
/// into the same directory rotate naturally instead of colliding. Any other path is
/// used as given. An existing file is refused rather than overwritten: the file
/// being replaced is the operator's previous good backup.
fn resolve_destination(destination: &Path) -> Result<PathBuf, Error> {
    let destination = if destination.is_dir() {
        let now = time::OffsetDateTime::now_utc();
        let timestamp = format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second(),
        );
        destination.join(format!("zallet-wallet-backup-{timestamp}.db"))
    } else {
        destination.to_path_buf()
    };

    if destination.exists() {
        return Err(ErrorKind::Generic
            .context(fl!(
                "err-backup-destination-exists",
                path = destination.display().to_string(),
            ))
            .into());
    }

    Ok(destination)
}

/// Copies the wallet database to `destination` using SQLite's Online Backup API,
/// returning the number of pages copied.
///
/// The copy lands in a temporary file beside the destination and is renamed into
/// place only after it has been verified and synced to disk, so `destination`
/// either does not exist or is a complete, checked backup — never a torn copy.
fn copy_database(source: &Path, destination: &Path) -> Result<u64, Error> {
    let tmp = tmp_path(destination);

    // Create the temporary file up front with owner-only permissions, mirroring
    // `Database::open`: the backup contains everything the wallet database does,
    // so it must never exist with wider permissions.
    create_private_file(&tmp)?;

    let result = copy_into(source, &tmp);

    match result {
        Ok(pages) => {
            durable_rename(&tmp, destination)?;
            Ok(pages)
        }
        Err(e) => {
            // Leave nothing behind on failure; a partial file that looks like a
            // backup is worse than no file.
            let _ = fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// The in-progress file the backup is written to before its durable rename.
fn tmp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".partial");
    destination.with_file_name(name)
}

fn create_private_file(path: &Path) -> Result<(), Error> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map(|_| ())
        .map_err(|e| ErrorKind::Generic.context(e).into())
}

/// Runs the Online Backup API copy and verifies the result, returning pages copied.
fn copy_into(source: &Path, tmp: &Path) -> Result<u64, Error> {
    let src = Connection::open_with_flags(
        source,
        // Read-only, and never create: this must observe the wallet database
        // without initializing or migrating anything, unlike `Database::open`.
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| ErrorKind::Generic.context(e))?;
    let mut dst = Connection::open(tmp).map_err(|e| ErrorKind::Generic.context(e))?;

    let pages = {
        let backup = Backup::new(&src, &mut dst).map_err(|e| ErrorKind::Generic.context(e))?;

        let mut busy_steps = 0;
        loop {
            match backup.step(PAGES_PER_STEP) {
                Ok(StepResult::Done) => break,
                // Progress means the busy state (if any) cleared, so the retry
                // limit bounds a consecutive busy streak rather than the total
                // across a large copy.
                Ok(StepResult::More) => busy_steps = 0,
                // `StepResult` is non-exhaustive; treat anything unrecognized like a
                // busy state so it is bounded by the same retry limit rather than
                // looping or aborting on a library addition.
                Ok(_) => {
                    busy_steps += 1;
                    if busy_steps > MAX_BUSY_STEPS {
                        return Err(ErrorKind::Generic
                            .context(fl!("err-backup-not-completing"))
                            .into());
                    }
                    thread::sleep(BUSY_RETRY_PAUSE);
                }
                Err(e) => return Err(ErrorKind::Generic.context(e).into()),
            }
        }

        let progress = backup.progress();
        u64::try_from(progress.pagecount).unwrap_or_default()
    };

    // A backup that cannot pass an integrity check is not a backup. `quick_check`
    // catches malformed pages and index corruption without `integrity_check`'s
    // full-table scans.
    let ok: String = dst
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| ErrorKind::Generic.context(e))?;
    if ok != "ok" {
        return Err(ErrorKind::Generic
            .context(fl!("err-backup-integrity", detail = ok))
            .into());
    }

    drop(dst);

    // The rename below is only meaningful if the file contents reach disk first.
    // The handle must be writable: on Windows, flushing requires write access.
    fs::OpenOptions::new()
        .write(true)
        .open(tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| ErrorKind::Generic.context(e))?;

    Ok(pages)
}

/// Renames `tmp` into place and syncs the parent directory, so the completed
/// backup survives a crash immediately after this command reports success.
fn durable_rename(tmp: &Path, destination: &Path) -> Result<(), Error> {
    fs::rename(tmp, destination).map_err(|e| ErrorKind::Generic.context(e))?;

    #[cfg(unix)]
    if let Some(parent) = destination.parent() {
        let parent = if parent.as_os_str().is_empty() {
            Path::new(".")
        } else {
            parent
        };
        fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| ErrorKind::Generic.context(e))?;
    }

    Ok(())
}

impl Runnable for BackupCmd {
    fn run(&self) {
        self.run_on_runtime();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded_db(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("source db opens");
        conn.execute_batch(
            "CREATE TABLE t (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
             INSERT INTO t (data) VALUES (randomblob(4096));
             INSERT INTO t (data) VALUES (randomblob(4096));",
        )
        .expect("schema and rows apply");
        conn
    }

    #[test]
    fn backup_copies_identical_content_and_passes_quick_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("wallet.db");
        let dest = dir.path().join("backup.db");

        let conn = seeded_db(&source);

        let pages = copy_database(&source, &dest).expect("backup succeeds");
        assert!(pages > 0, "a non-empty database copies at least one page");
        assert!(dest.exists(), "the backup file is renamed into place");
        assert!(
            !tmp_path(&dest).exists(),
            "the temporary file does not outlive the backup",
        );

        let copy = Connection::open_with_flags(&dest, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .expect("backup opens");
        let (rows, bytes): (u64, u64) = copy
            .query_row("SELECT COUNT(*), SUM(LENGTH(data)) FROM t", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .expect("backup is queryable");
        assert_eq!(rows, 2);
        assert_eq!(bytes, 8192);

        drop(conn);
    }

    #[cfg(unix)]
    #[test]
    fn backup_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("wallet.db");
        let dest = dir.path().join("backup.db");

        let _conn = seeded_db(&source);
        copy_database(&source, &dest).expect("backup succeeds");

        let mode = fs::metadata(&dest).expect("metadata").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "backup must be private to the owner");
    }

    #[test]
    fn directory_destination_gets_a_timestamped_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");

        let resolved = resolve_destination(dir.path()).expect("resolves");
        let name = resolved.file_name().unwrap().to_string_lossy();
        assert!(
            name.starts_with("zallet-wallet-backup-") && name.ends_with(".db"),
            "unexpected generated name: {name}",
        );
        assert_eq!(resolved.parent().unwrap(), dir.path());
    }

    #[test]
    fn existing_destination_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("backup.db");
        fs::write(&dest, b"previous good backup").expect("existing file");

        assert!(
            resolve_destination(&dest).is_err(),
            "an existing backup must never be overwritten",
        );
        assert_eq!(
            fs::read(&dest).expect("still readable"),
            b"previous good backup",
        );
    }

    #[test]
    fn failed_backup_leaves_no_partial_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let source = dir.path().join("missing.db");
        let dest = dir.path().join("backup.db");

        assert!(copy_database(&source, &dest).is_err());
        assert!(!dest.exists());
        assert!(!tmp_path(&dest).exists());
    }
}
