//! Append-only JSONL storage for [`AuditEvent`]s.

use std::{
    fs::{self, DirBuilder, File, OpenOptions, Permissions},
    io::{self, ErrorKind},
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, FileExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::error::ModelError;

use super::{
    event::{AuditEvent, AuditKind, MAX_LAB_LEN},
    filter::AuditFilter,
};

/// Directory holding one subdirectory per lab.
const LABS_DIR: &str = "labs";
/// File name of the audit log inside a lab directory.
const AUDIT_FILE: &str = "audit.jsonl";
/// Permission bits of a lab audit directory.
const DIR_MODE: u32 = 0o700;
/// Permission bits of the audit log file.
const FILE_MODE: u32 = 0o600;
/// First amount of bytes read when looking for the chain tip.
const TIP_CHUNK: u64 = 64 * 1024;

/// Append-only log of one lab.
///
/// The log is the single file `<dir>/labs/<lab>/audit.jsonl`, where `dir` is the data
/// directory (normally `paths::data_dir()`). Opening refuses symlinks and non-regular files
/// and keeps the lab directory at mode 0700 and the file at mode 0600.
///
/// Every append takes an exclusive advisory `flock` on the file, reads the chain tip and
/// writes one newline-terminated JSON line with a single `write(2)` call, so several
/// processes can append to the same lab log without interleaving lines. Events are never
/// rewritten or removed.
#[derive(Debug)]
pub struct AuditLog {
    path: PathBuf,
    lab: String,
    file: Mutex<File>,
}

impl AuditLog {
    /// Opens the log of `lab` under `dir`, creating directories and file when needed.
    ///
    /// `lab` is recorded in every event and must be a single non-empty path segment.
    pub fn open(dir: impl Into<PathBuf>, lab: impl Into<String>) -> Result<Self, ModelError> {
        let lab = lab_segment(&lab.into())?;
        let labs = dir.into().join(LABS_DIR);
        ensure_private_dir(&labs)?;
        let dir = labs.join(&lab);
        ensure_private_dir(&dir)?;
        let path = dir.join(AUDIT_FILE);
        let file = open_log_file(&path)?;
        Ok(Self {
            path,
            lab,
            file: Mutex::new(file),
        })
    }

    /// Path of the JSONL file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lab name recorded in every event.
    pub fn lab(&self) -> &str {
        &self.lab
    }

    /// Appends one event and returns it with `seq`, `prev_hash` and `hash` set.
    pub fn append(
        &self,
        project: Option<&str>,
        worker: Option<&str>,
        kind: AuditKind,
    ) -> Result<AuditEvent, ModelError> {
        let file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _lock = FileLock::exclusive(&file, &self.path)?;

        let (last_seq, prev_hash) = read_chain_tip(&file, &self.path, &self.lab)?;
        let seq = last_seq
            .checked_add(1)
            .ok_or_else(|| corrupt(&self.path, "sequence number overflow"))?;
        let event = AuditEvent::new(
            seq,
            now_ms(),
            self.lab.clone(),
            project.map(str::to_string),
            worker.map(str::to_string),
            kind,
            prev_hash,
        )?;

        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        write_line(&file, &line, &self.path)?;
        Ok(event)
    }

    /// Every event of the log, in sequence order.
    ///
    /// A line that does not parse, including a torn last line, is reported as
    /// [`ModelError::AuditCorrupt`] instead of being skipped.
    pub fn read_all(&self) -> Result<Vec<AuditEvent>, ModelError> {
        let bytes = self.read_bytes()?;
        parse_lines(&bytes, &self.path)
    }

    /// An iterator over every event of the log, in sequence order.
    pub fn iter(&self) -> Result<std::vec::IntoIter<AuditEvent>, ModelError> {
        Ok(self.read_all()?.into_iter())
    }

    /// Events selected by `filter`, in sequence order.
    pub fn read(&self, filter: &AuditFilter) -> Result<Vec<AuditEvent>, ModelError> {
        let events: Vec<AuditEvent> = self
            .read_all()?
            .into_iter()
            .filter(|event| filter.matches(event))
            .collect();
        Ok(filter.apply_tail(events))
    }

    /// Checks the hash chain and returns the first sequence number that does not fit.
    ///
    /// `Ok(None)` means every event is intact: sequence numbers start at 1 and grow by
    /// one, each `prev_hash` is the previous event's `hash`, and every `hash` matches its
    /// contents. `Ok(Some(seq))` is the chain position of the first change, which also
    /// covers a removed line and a torn last line. I/O failures are errors.
    pub fn verify(&self) -> Result<Option<u64>, ModelError> {
        let bytes = self.read_bytes()?;
        let mut expected_seq = 1u64;
        let mut prev_hash = String::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let Some(position) = rest.iter().position(|&byte| byte == b'\n') else {
                // A torn last line never became an event, so it breaks the chain where
                // that event should have been.
                return Ok(Some(expected_seq));
            };
            let line = &rest[..position];
            rest = &rest[position + 1..];

            let Ok(event) = serde_json::from_slice::<AuditEvent>(line) else {
                return Ok(Some(expected_seq));
            };
            if event.seq != expected_seq
                || event.lab != self.lab
                || event.prev_hash != prev_hash
                || event.compute_hash()? != event.hash
            {
                return Ok(Some(expected_seq));
            }
            expected_seq = expected_seq
                .checked_add(1)
                .ok_or_else(|| corrupt(&self.path, "sequence number overflow"))?;
            prev_hash = event.hash;
        }
        Ok(None)
    }

    /// Snapshot of the log file, read under a shared lock so a concurrent append is not
    /// seen half-written.
    fn read_bytes(&self) -> Result<Vec<u8>, ModelError> {
        let file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let _lock = FileLock::shared(&file, &self.path)?;
        let len = file
            .metadata()
            .map_err(|source| ModelError::io(&self.path, source))?
            .len();
        let len = usize::try_from(len)
            .map_err(|_| corrupt(&self.path, "the log is too large to read"))?;
        let mut bytes = vec![0u8; len];
        file.read_exact_at(&mut bytes, 0)
            .map_err(|source| ModelError::io(&self.path, source))?;
        Ok(bytes)
    }
}

/// Exclusive advisory lock on an open log file, released on drop.
struct FileLock<'a> {
    file: &'a File,
}

impl<'a> FileLock<'a> {
    fn exclusive(file: &'a File, path: &Path) -> Result<Self, ModelError> {
        Self::acquire(file, path, libc::LOCK_EX)
    }

    fn shared(file: &'a File, path: &Path) -> Result<Self, ModelError> {
        Self::acquire(file, path, libc::LOCK_SH)
    }

    fn acquire(file: &'a File, path: &Path, operation: libc::c_int) -> Result<Self, ModelError> {
        loop {
            // SAFETY: `file` holds a valid descriptor for the lifetime of the lock.
            let result = unsafe { libc::flock(file.as_raw_fd(), operation) };
            if result == 0 {
                return Ok(Self { file });
            }
            let source = io::Error::last_os_error();
            if source.kind() != ErrorKind::Interrupted {
                return Err(ModelError::io(path, source));
            }
        }
    }
}

impl Drop for FileLock<'_> {
    fn drop(&mut self) {
        // SAFETY: the descriptor stays open until this borrow ends.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

fn ensure_private_dir(path: &Path) -> Result<(), ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(ModelError::UnsafeAuditPath(path.to_path_buf()));
        }
        Ok(_) => {}
        Err(source) if source.kind() == ErrorKind::NotFound => {
            DirBuilder::new()
                .recursive(true)
                .mode(DIR_MODE)
                .create(path)
                .map_err(|source| ModelError::io(path, source))?;
        }
        Err(source) => return Err(ModelError::io(path, source)),
    }
    fs::set_permissions(path, Permissions::from_mode(DIR_MODE))
        .map_err(|source| ModelError::io(path, source))
}

fn open_log_file(path: &Path) -> Result<File, ModelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.file_type().is_file() => {
            return Err(ModelError::UnsafeAuditPath(path.to_path_buf()));
        }
        Ok(_) => {}
        Err(source) if source.kind() == ErrorKind::NotFound => {}
        Err(source) => return Err(ModelError::io(path, source)),
    }

    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(FILE_MODE)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|source| open_error(path, source))?;

    let metadata = file
        .metadata()
        .map_err(|source| ModelError::io(path, source))?;
    if !metadata.is_file() {
        return Err(ModelError::UnsafeAuditPath(path.to_path_buf()));
    }
    file.set_permissions(Permissions::from_mode(FILE_MODE))
        .map_err(|source| ModelError::io(path, source))?;
    Ok(file)
}

fn open_error(path: &Path, source: io::Error) -> ModelError {
    if source.raw_os_error() == Some(libc::ELOOP) {
        ModelError::UnsafeAuditPath(path.to_path_buf())
    } else {
        ModelError::io(path, source)
    }
}

/// Validates that `lab` can be used as one directory name.
fn lab_segment(lab: &str) -> Result<String, ModelError> {
    if lab.is_empty() {
        return Err(ModelError::InvalidAuditField {
            field: "lab",
            reason: "must not be empty".to_string(),
        });
    }
    if lab.len() > MAX_LAB_LEN {
        return Err(ModelError::InvalidAuditField {
            field: "lab",
            reason: format!("must be at most {MAX_LAB_LEN} bytes"),
        });
    }
    if lab == "." || lab == ".." || lab.contains('/') || lab.contains('\\') || lab.contains('\0') {
        return Err(ModelError::InvalidAuditField {
            field: "lab",
            reason: "must be a single path segment".to_string(),
        });
    }
    Ok(lab.to_string())
}

/// Last `seq` and `hash` of the log, or `(0, "")` when the file is empty.
///
/// A corrupt tail is an error, not a place to append: continuing a broken chain would hide
/// the damage.
fn read_chain_tip(file: &File, path: &Path, lab: &str) -> Result<(u64, String), ModelError> {
    let len = file
        .metadata()
        .map_err(|source| ModelError::io(path, source))?
        .len();
    if len == 0 {
        return Ok((0, String::new()));
    }

    let mut chunk = TIP_CHUNK.min(len);
    loop {
        let start = len - chunk;
        let mut tail = vec![0u8; (len - start) as usize];
        file.read_exact_at(&mut tail, start)
            .map_err(|source| ModelError::io(path, source))?;

        if tail.last() != Some(&b'\n') {
            return Err(corrupt(
                path,
                "the last line does not end with a newline (torn append)",
            ));
        }
        let body = &tail[..tail.len() - 1];
        let line = match body.iter().rposition(|&byte| byte == b'\n') {
            Some(position) => &body[position + 1..],
            None if start == 0 => body,
            None => {
                chunk = chunk.saturating_mul(2).min(len);
                continue;
            }
        };
        if line.is_empty() {
            return Err(corrupt(path, "the last line is empty"));
        }
        return parse_tip(line, path, lab);
    }
}

fn parse_tip(line: &[u8], path: &Path, lab: &str) -> Result<(u64, String), ModelError> {
    let event: AuditEvent =
        serde_json::from_slice(line).map_err(|source| ModelError::AuditCorrupt {
            path: path.to_path_buf(),
            detail: format!("the last line is not a valid audit event: {source}"),
        })?;
    if event.lab != lab {
        return Err(corrupt(
            path,
            format!("the last event belongs to lab {:?}", event.lab),
        ));
    }
    let expected = event.compute_hash()?;
    if expected != event.hash {
        return Err(corrupt(
            path,
            format!(
                "the last event (seq {}) hash does not match its contents",
                event.seq
            ),
        ));
    }
    Ok((event.seq, event.hash))
}

/// Appends `line` with one `write(2)` call, retrying only when interrupted before writing.
fn write_line(file: &File, line: &[u8], path: &Path) -> Result<(), ModelError> {
    loop {
        // SAFETY: `line` is a live buffer and `file` holds a valid descriptor.
        let written = unsafe { libc::write(file.as_raw_fd(), line.as_ptr().cast(), line.len()) };
        if written < 0 {
            let source = io::Error::last_os_error();
            if source.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(ModelError::io(path, source));
        }
        if written as usize != line.len() {
            return Err(ModelError::io(
                path,
                io::Error::new(
                    ErrorKind::WriteZero,
                    "short write while appending an audit event",
                ),
            ));
        }
        return Ok(());
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Parses every newline-terminated line, reporting the first line that does not fit.
fn parse_lines(bytes: &[u8], path: &Path) -> Result<Vec<AuditEvent>, ModelError> {
    let mut events = Vec::new();
    let mut rest = bytes;
    let mut line_number = 0u64;
    while !rest.is_empty() {
        line_number += 1;
        let Some(position) = rest.iter().position(|&byte| byte == b'\n') else {
            return Err(corrupt(
                path,
                format!("line {line_number} is torn: it does not end with a newline"),
            ));
        };
        let line = &rest[..position];
        rest = &rest[position + 1..];
        if line.is_empty() {
            return Err(corrupt(path, format!("line {line_number} is empty")));
        }
        let event = serde_json::from_slice(line).map_err(|source| {
            corrupt(
                path,
                format!("line {line_number} is not a valid audit event: {source}"),
            )
        })?;
        events.push(event);
    }
    Ok(events)
}

fn corrupt(path: &Path, detail: impl Into<String>) -> ModelError {
    ModelError::AuditCorrupt {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        fs,
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::{
            Arc, Barrier,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use super::*;
    use crate::audit::{AuditFilter, CheckOutcome, CheckStatus};

    /// Temporary lab directory removed when the test ends.
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("jgf-core-audit-{}-{suffix}", std::process::id()));
            fs::create_dir_all(&path).expect("create temp root");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    fn lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .expect("read log")
            .lines()
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn append_writes_a_private_hash_chain() {
        let root = TempRoot::new();
        let dir = root.path().join(LABS_DIR).join("alpha");
        let log = AuditLog::open(root.path(), "alpha").expect("open log");
        assert_eq!(log.path(), dir.join(AUDIT_FILE));

        let first = log
            .append(
                Some("demo"),
                Some("worker-1"),
                AuditKind::command(
                    vec!["cargo".into(), "test".into()],
                    Some(0),
                    None,
                    false,
                    12,
                ),
            )
            .expect("append first");
        let second = log
            .append(
                Some("demo"),
                None,
                AuditKind::board_move(
                    "JGT-1",
                    "todo",
                    "doing",
                    "dev",
                    "start",
                    vec![CheckStatus::new("tests", CheckOutcome::Pass)],
                ),
            )
            .expect("append second");

        assert_eq!(first.seq, 1);
        assert_eq!(first.prev_hash, "");
        assert_eq!(first.hash, first.compute_hash().expect("hash"));
        assert_eq!(second.seq, 2);
        assert_eq!(second.prev_hash, first.hash);
        assert_eq!(second.hash, second.compute_hash().expect("hash"));

        assert_eq!(mode(&dir), DIR_MODE);
        assert_eq!(mode(log.path()), FILE_MODE);

        let lines = lines(log.path());
        assert_eq!(lines.len(), 2);
        let stored: Vec<AuditEvent> = lines
            .iter()
            .map(|line| serde_json::from_str(line).expect("valid event"))
            .collect();
        assert_eq!(stored, vec![first, second]);
        assert_eq!(stored[1].kind.tag(), "board_move");
    }

    #[test]
    fn reopening_continues_the_chain() {
        let root = TempRoot::new();
        let first = {
            let log = AuditLog::open(root.path(), "alpha").expect("open log");
            log.append(
                None,
                None,
                AuditKind::network("example.com", 443, true, "allowed"),
            )
            .expect("append")
        };

        let log = AuditLog::open(root.path(), "alpha").expect("reopen log");
        let second = log
            .append(None, None, AuditKind::cost("worker-1", 10, 2, 3))
            .expect("append");

        assert_eq!(second.seq, first.seq + 1);
        assert_eq!(second.prev_hash, first.hash);
        assert_eq!(lines(log.path()).len(), 2);
    }

    #[test]
    fn open_refuses_a_symlinked_log() {
        let root = TempRoot::new();
        let dir = root.path().join(LABS_DIR).join("alpha");
        fs::create_dir_all(&dir).expect("create dir");
        let target = root.path().join("elsewhere");
        fs::write(&target, "").expect("target");
        symlink(&target, dir.join(AUDIT_FILE)).expect("symlink");

        let error = AuditLog::open(root.path(), "alpha").expect_err("must refuse symlink");
        assert!(matches!(error, ModelError::UnsafeAuditPath(_)), "{error:?}");
    }

    #[test]
    fn open_refuses_a_symlinked_labs_dir() {
        let root = TempRoot::new();
        let elsewhere = root.path().join("elsewhere");
        fs::create_dir_all(&elsewhere).expect("create target dir");
        symlink(&elsewhere, root.path().join(LABS_DIR)).expect("symlink");

        let error = AuditLog::open(root.path(), "alpha").expect_err("must refuse symlink");
        assert!(matches!(error, ModelError::UnsafeAuditPath(_)), "{error:?}");
    }

    #[test]
    fn open_refuses_a_non_regular_log() {
        let root = TempRoot::new();
        let dir = root.path().join(LABS_DIR).join("alpha");
        fs::create_dir_all(dir.join(AUDIT_FILE)).expect("create log directory");

        let error = AuditLog::open(root.path(), "alpha").expect_err("must refuse directory");
        assert!(matches!(error, ModelError::UnsafeAuditPath(_)), "{error:?}");
    }

    #[test]
    fn append_refuses_a_corrupt_tail() {
        let root = TempRoot::new();
        let log = AuditLog::open(root.path(), "alpha").expect("open log");
        fs::write(log.path(), "not json\n").expect("write corrupt tail");

        let error = log
            .append(None, None, AuditKind::cost("worker-1", 1, 1, 1))
            .expect_err("must refuse corrupt tail");
        assert!(
            matches!(error, ModelError::AuditCorrupt { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn invalid_lab_names_are_rejected() {
        let root = TempRoot::new();
        let too_long = "a".repeat(MAX_LAB_LEN + 1);
        for lab in [
            "",
            ".",
            "..",
            "../alpha",
            "alpha/beta",
            "alpha\\beta",
            too_long.as_str(),
        ] {
            let error = AuditLog::open(root.path(), lab).expect_err("must reject lab");
            assert!(
                matches!(error, ModelError::InvalidAuditField { field: "lab", .. }),
                "{lab:?}: {error:?}"
            );
        }
    }

    /// Appends `count` cost events and returns the log.
    fn seeded_log(root: &Path, count: usize) -> AuditLog {
        let log = AuditLog::open(root, "alpha").expect("open log");
        for index in 0..count {
            log.append(
                Some("demo"),
                Some("worker-1"),
                AuditKind::cost("worker-1", 10 + index as u64, 1, 1),
            )
            .expect("append");
        }
        log
    }

    fn seqs(events: &[AuditEvent]) -> Vec<u64> {
        events.iter().map(|event| event.seq).collect()
    }

    fn rewrite(path: &Path, lines: &[String]) {
        fs::write(path, format!("{}\n", lines.join("\n"))).expect("rewrite log");
    }

    #[test]
    fn read_all_and_iter_return_the_chain_in_order() {
        let root = TempRoot::new();
        let log = AuditLog::open(root.path(), "alpha").expect("open log");
        assert!(log.read_all().expect("read empty log").is_empty());
        assert_eq!(log.verify().expect("verify empty log"), None);

        let first = log
            .append(
                None,
                None,
                AuditKind::network("example.com", 443, true, "ok"),
            )
            .expect("append");
        let second = log
            .append(None, None, AuditKind::cost("worker-1", 5, 2, 3))
            .expect("append");

        assert_eq!(log.read_all().expect("read all"), vec![first, second]);
        let iterated: Vec<u64> = log.iter().expect("iter").map(|event| event.seq).collect();
        assert_eq!(iterated, [1, 2]);
    }

    #[test]
    fn filters_select_by_project_worker_and_kind() {
        let root = TempRoot::new();
        let log = AuditLog::open(root.path(), "alpha").expect("open log");
        log.append(
            Some("demo"),
            Some("worker-1"),
            AuditKind::command(vec!["cargo".into()], Some(0), None, false, 5),
        )
        .expect("append");
        log.append(
            Some("demo"),
            Some("worker-2"),
            AuditKind::network("example.com", 443, true, "allowed"),
        )
        .expect("append");
        log.append(
            Some("other"),
            Some("worker-1"),
            AuditKind::cost("worker-1", 1, 1, 1),
        )
        .expect("append");
        log.append(None, None, AuditKind::denied_write("/tmp/x", "outside"))
            .expect("append");

        let demo = log
            .read(&AuditFilter::new().project("demo"))
            .expect("read demo");
        assert_eq!(seqs(&demo), [1, 2]);

        let worker = log
            .read(&AuditFilter::new().worker("worker-1"))
            .expect("read worker");
        assert_eq!(seqs(&worker), [1, 3]);

        let cost = log
            .read(&AuditFilter::new().kind("cost"))
            .expect("read cost");
        assert_eq!(seqs(&cost), [3]);

        let combined = log
            .read(&AuditFilter::new().project("demo").kind("network"))
            .expect("read combined");
        assert_eq!(seqs(&combined), [2]);

        let unmatched = log
            .read(&AuditFilter::new().project("demo").worker("worker-1"))
            .expect("read unmatched");
        assert_eq!(seqs(&unmatched), [1]);
    }

    #[test]
    fn time_range_filter_is_inclusive() {
        let event = AuditEvent::new(
            1,
            1_500,
            "alpha",
            None,
            None,
            AuditKind::cost("worker-1", 1, 1, 1),
            "",
        )
        .expect("valid event");

        assert!(AuditFilter::new().from_ms(1_500).matches(&event));
        assert!(AuditFilter::new().to_ms(1_500).matches(&event));
        assert!(!AuditFilter::new().from_ms(1_501).matches(&event));
        assert!(!AuditFilter::new().to_ms(1_499).matches(&event));
        assert!(
            AuditFilter::new()
                .from_ms(1_000)
                .to_ms(2_000)
                .matches(&event)
        );
        assert!(AuditFilter::new().matches(&event));
    }

    #[test]
    fn tail_keeps_the_last_events() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 5);

        let last_two = log.read(&AuditFilter::new().tail(2)).expect("read tail");
        assert_eq!(seqs(&last_two), [4, 5]);

        let none = log
            .read(&AuditFilter::new().tail(0))
            .expect("read empty tail");
        assert!(none.is_empty());

        let all = log
            .read(&AuditFilter::new().tail(99))
            .expect("read large tail");
        assert_eq!(seqs(&all), [1, 2, 3, 4, 5]);
    }

    #[test]
    fn verify_accepts_an_intact_chain() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 3);

        assert_eq!(log.verify().expect("verify intact log"), None);
    }

    #[test]
    fn verify_reports_the_first_changed_event() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 3);

        let mut stored: Vec<serde_json::Value> = lines(log.path())
            .iter()
            .map(|line| serde_json::from_str(line).expect("valid event"))
            .collect();
        stored[1]["kind"]["usd_micros"] = serde_json::json!(9_999);
        let tampered: Vec<String> = stored
            .iter()
            .map(|value| serde_json::to_string(value).expect("json"))
            .collect();
        rewrite(log.path(), &tampered);

        assert_eq!(log.verify().expect("verify changed log"), Some(2));
        assert_eq!(log.read_all().expect("read changed log").len(), 3);
    }

    #[test]
    fn verify_reports_a_changed_hash() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 3);

        let mut stored: Vec<serde_json::Value> = lines(log.path())
            .iter()
            .map(|line| serde_json::from_str(line).expect("valid event"))
            .collect();
        stored[0]["hash"] = serde_json::json!("0".repeat(64));
        let tampered: Vec<String> = stored
            .iter()
            .map(|value| serde_json::to_string(value).expect("json"))
            .collect();
        rewrite(log.path(), &tampered);

        assert_eq!(log.verify().expect("verify changed hash"), Some(1));
    }

    #[test]
    fn verify_reports_a_removed_event() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 3);

        let stored = lines(log.path());
        rewrite(log.path(), &[stored[0].clone(), stored[2].clone()]);

        assert_eq!(log.verify().expect("verify shortened log"), Some(2));
    }

    #[test]
    fn torn_last_line_is_reported() {
        let root = TempRoot::new();
        let log = seeded_log(root.path(), 1);

        let mut raw = fs::OpenOptions::new()
            .append(true)
            .open(log.path())
            .expect("open raw log");
        raw.write_all(b"{\"seq\":2").expect("write torn line");
        drop(raw);

        let error = log.read_all().expect_err("torn line must be reported");
        assert!(
            matches!(error, ModelError::AuditCorrupt { .. }),
            "{error:?}"
        );
        assert_eq!(log.verify().expect("verify torn log"), Some(2));
    }

    #[test]
    fn concurrent_writers_keep_the_chain_consistent() {
        const WRITERS: usize = 8;
        const PER_WRITER: usize = 25;
        let root = TempRoot::new();
        let barrier = Arc::new(Barrier::new(WRITERS));

        let mut handles = Vec::with_capacity(WRITERS);
        for writer in 0..WRITERS {
            let dir = root.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                let log = AuditLog::open(dir, "alpha").expect("open log");
                let worker = format!("worker-{writer}");
                barrier.wait();
                for index in 0..PER_WRITER {
                    log.append(
                        Some("demo"),
                        Some(&worker),
                        AuditKind::command(
                            vec![format!("cmd-{writer}-{index}")],
                            Some(0),
                            None,
                            false,
                            1,
                        ),
                    )
                    .expect("append");
                }
            }));
        }
        for handle in handles {
            handle.join().expect("writer thread");
        }

        let log = AuditLog::open(root.path(), "alpha").expect("reopen log");
        assert_eq!(log.verify().expect("verify concurrent log"), None);
        let events = log.read_all().expect("read concurrent log");
        assert_eq!(events.len(), WRITERS * PER_WRITER);
        assert_eq!(
            seqs(&events),
            (1..=events.len() as u64).collect::<Vec<u64>>()
        );
        let workers: BTreeSet<&str> = events
            .iter()
            .filter_map(|event| event.worker.as_deref())
            .collect();
        assert_eq!(workers.len(), WRITERS);
    }
}
