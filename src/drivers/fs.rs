//! The `fs.watch` observer: read-only directory snapshot diffing.
//!
//! [`Observer::start`] records a bounded snapshot of the files inside the
//! watched directory and [`Observer::collect`] takes a second snapshot,
//! reporting deterministic sorted `created`, `modified`, and `deleted` relative
//! paths using `/` separators. Nothing is ever created or written, and
//! directory symlinks are never followed.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use schemars::{JsonSchema, schema_for};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::core::observer::{Observer, ObserverDefinition, ObserverError};
use crate::core::paths;
use crate::core::registry::Registry;
use crate::core::session::Sessions;

/// Name of the observer this module registers.
pub const OBSERVER_NAME: &str = "fs.watch";

/// Default cap on scanned entries per directory snapshot.
pub const DEFAULT_MAX_ENTRIES: u64 = 1_000;

const READ_CHUNK: usize = 8 * 1024;

/// Registers the observers of this module.
pub fn register(registry: &mut Registry) -> Result<(), String> {
    let args_schema = serde_json::to_value(schema_for!(FsWatchArgs))
        .map_err(|error| format!("could not build the schema of `{OBSERVER_NAME}`: {error}"))?;
    registry.register_observer(ObserverDefinition {
        name: OBSERVER_NAME,
        description: "Reports files created, modified, and deleted inside a directory while the step ran.",
        args_schema,
        validate,
        create,
    })
}

/// Arguments of `fs.watch`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FsWatchArgs {
    /// Directory path to watch.
    // The schema enforces the same non-empty rule as `check`.
    #[schemars(extend("minLength" = 1))]
    path: String,
    /// Whether subdirectories are walked recursively. Defaults to `true`.
    #[serde(default = "default_recursive")]
    recursive: bool,
    /// Maximum number of entries allowed in one snapshot. Defaults to 1000.
    #[schemars(extend("minimum" = 1))]
    #[serde(default = "default_max_entries")]
    max_entries: u64,
    /// Compute SHA-256 content digests instead of metadata-only comparisons. Defaults to `false`.
    #[serde(default)]
    hash: bool,
}

fn default_recursive() -> bool {
    true
}

fn default_max_entries() -> u64 {
    DEFAULT_MAX_ENTRIES
}

fn validate(args: &Value) -> Result<(), String> {
    check(&parse(args)?)
}

fn parse(args: &Value) -> Result<FsWatchArgs, String> {
    serde_json::from_value(args.clone()).map_err(|error| format!("invalid arguments: {error}"))
}

fn check(args: &FsWatchArgs) -> Result<(), String> {
    if args.path.is_empty() {
        return Err("invalid arguments: `path` must name a directory".to_string());
    }
    if args.max_entries == 0 {
        return Err("invalid arguments: `max_entries` must be at least 1".to_string());
    }
    Ok(())
}

fn create(args: &Value) -> Result<Box<dyn Observer>, ObserverError> {
    let args = parse(args).map_err(ObserverError::InvalidArguments)?;
    check(&args).map_err(ObserverError::InvalidArguments)?;
    Ok(Box::new(FsWatch {
        path: PathBuf::from(args.path),
        recursive: args.recursive,
        max_entries: args.max_entries,
        hash: args.hash,
        baseline: BTreeMap::new(),
    }))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FileFingerprint {
    Metadata {
        len: u64,
        modified: Option<SystemTime>,
    },
    Digest {
        len: u64,
        sha256: [u8; 32],
    },
}

/// One case's read-only directory snapshot observer.
struct FsWatch {
    path: PathBuf,
    recursive: bool,
    max_entries: u64,
    hash: bool,
    baseline: BTreeMap<String, FileFingerprint>,
}

impl Observer for FsWatch {
    fn start(&mut self, _sessions: &mut Sessions) -> Result<(), ObserverError> {
        self.baseline = snapshot_tree(&self.path, self.recursive, self.max_entries, self.hash)?;
        Ok(())
    }

    fn collect(&mut self, _sessions: &mut Sessions) -> Result<Value, ObserverError> {
        let current = snapshot_tree(&self.path, self.recursive, self.max_entries, self.hash)?;
        let diff = diff_snapshots(&self.baseline, &current);
        // Advance the baseline even if a step runs between collections without
        // a matching `start`, matching `log.file`.
        self.baseline = current;
        Ok(diff)
    }
}

fn diff_snapshots(
    before: &BTreeMap<String, FileFingerprint>,
    after: &BTreeMap<String, FileFingerprint>,
) -> Value {
    let mut created = Vec::new();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();

    for (path, after_fp) in after {
        match before.get(path) {
            None => created.push(path.clone()),
            Some(before_fp) if before_fp != after_fp => modified.push(path.clone()),
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            deleted.push(path.clone());
        }
    }

    json!({
        "created": created,
        "modified": modified,
        "deleted": deleted,
    })
}

fn snapshot_tree(
    root: &Path,
    recursive: bool,
    max_entries: u64,
    hash: bool,
) -> Result<BTreeMap<String, FileFingerprint>, ObserverError> {
    // Follow symlinks on `root` itself (macOS `/tmp` and `/var` are symlinks),
    // while child traversal inspects `file_type()` so internal directory
    // symlinks never create cycles.
    match std::fs::metadata(root) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(not_a_directory(root)),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(error) => return Err(environment(root, &error)),
    }

    let mut files = BTreeMap::new();
    let mut stack = vec![(root.to_path_buf(), String::new())];
    let mut entries_seen: u64 = 0;

    while let Some((dir_path, rel_prefix)) = stack.pop() {
        let read_dir = match std::fs::read_dir(&dir_path) {
            Ok(iter) => iter,
            Err(error) if error.kind() == ErrorKind::NotFound => continue,
            Err(error) => return Err(environment(&dir_path, &error)),
        };

        let mut children =
            collect_directory_entries(read_dir, &mut entries_seen, max_entries, root, &dir_path)?;
        children.sort_by_key(|entry| entry.file_name());

        let mut subdirs = Vec::new();
        for entry in children {
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) if error.kind() == ErrorKind::NotFound => continue,
                Err(error) => return Err(environment(&entry.path(), &error)),
            };

            if file_type.is_symlink() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().into_owned();
            let rel = if rel_prefix.is_empty() {
                name
            } else {
                format!("{rel_prefix}/{name}")
            };

            if file_type.is_dir() {
                if recursive {
                    subdirs.push((entry.path(), rel));
                }
            } else if file_type.is_file()
                && let Some(fp) = read_fingerprint(&entry.path(), hash)?
            {
                files.insert(rel, fp);
            }
        }

        // Push in reverse so sorted order is visited first-to-last.
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }

    Ok(files)
}

/// Reads one directory while counting every entry against the snapshot cap.
///
/// The cap is checked before an entry is retained, so a directory larger than
/// `max_entries` cannot allocate an arbitrary vector before failing.
fn collect_directory_entries<I>(
    entries: I,
    entries_seen: &mut u64,
    max_entries: u64,
    root: &Path,
    dir_path: &Path,
) -> Result<Vec<std::fs::DirEntry>, ObserverError>
where
    I: Iterator<Item = std::io::Result<std::fs::DirEntry>>,
{
    let mut children = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| environment(dir_path, &error))?;
        *entries_seen = entries_seen.saturating_add(1);
        if *entries_seen > max_entries {
            return Err(max_entries_error(root, max_entries));
        }
        children.push(entry);
    }
    Ok(children)
}

fn read_fingerprint(path: &Path, hash: bool) -> Result<Option<FileFingerprint>, ObserverError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(environment(path, &error)),
    };
    if !metadata.is_file() {
        return Ok(None);
    }
    let len = metadata.len();
    if !hash {
        return Ok(Some(FileFingerprint::Metadata {
            len,
            modified: metadata.modified().ok(),
        }));
    }

    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(environment(path, &error)),
    };
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; READ_CHUNK];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(environment(path, &error)),
        }
    }
    Ok(Some(FileFingerprint::Digest {
        len,
        sha256: hasher.finalize(),
    }))
}

fn environment(path: &Path, error: &std::io::Error) -> ObserverError {
    ObserverError::Environment(format!(
        "could not read `{}`: {error}",
        paths::display(path)
    ))
}

fn not_a_directory(path: &Path) -> ObserverError {
    ObserverError::Environment(format!(
        "could not watch `{}`: it is not a directory",
        paths::display(path)
    ))
}

fn max_entries_error(root: &Path, max_entries: u64) -> ObserverError {
    ObserverError::Environment(format!(
        "`{OBSERVER_NAME}` exceeded `max_entries` ({max_entries}) while scanning `{}`; narrow `path`, set `recursive: false`, or raise `max_entries`",
        paths::display(root)
    ))
}

/// Minimal streaming FIPS 180-4 SHA-256 implementation with zero `unsafe` and
/// zero third-party dependencies.
struct Sha256 {
    state: [u32; 8],
    len_bytes: u64,
    buffer: [u8; 64],
    buffer_len: usize,
}

impl Sha256 {
    const INIT: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    fn new() -> Self {
        Self {
            state: Self::INIT,
            len_bytes: 0,
            buffer: [0; 64],
            buffer_len: 0,
        }
    }

    fn update(&mut self, mut data: &[u8]) {
        self.len_bytes = self.len_bytes.wrapping_add(data.len() as u64);
        if self.buffer_len > 0 {
            let take = (64 - self.buffer_len).min(data.len());
            self.buffer[self.buffer_len..self.buffer_len + take].copy_from_slice(&data[..take]);
            self.buffer_len += take;
            data = &data[take..];
            if self.buffer_len == 64 {
                let block = self.buffer;
                self.compress(&block);
                self.buffer_len = 0;
            }
        }
        while data.len() >= 64 {
            let mut block = [0_u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    fn finalize(mut self) -> [u8; 32] {
        let bit_len = self.len_bytes.wrapping_mul(8);
        self.update(&[0x80]);
        if self.buffer_len > 56 {
            self.buffer[self.buffer_len..].fill(0);
            let block = self.buffer;
            self.compress(&block);
            self.buffer_len = 0;
        }
        self.buffer[self.buffer_len..56].fill(0);
        self.buffer[56..64].copy_from_slice(&bit_len.to_be_bytes());
        let block = self.buffer;
        self.compress(&block);

        let mut out = [0_u8; 32];
        for (i, word) in self.state.iter().enumerate() {
            out[i * 4..(i + 1) * 4].copy_from_slice(&word.to_be_bytes());
        }
        out
    }

    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0_u32; 64];
        for (dest, chunk) in w[..16].iter_mut().zip(block.as_chunks::<4>().0) {
            *dest = u32::from_be_bytes(*chunk);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for (&k, &word) in Self::K.iter().zip(w.iter()) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(word);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        let vals = [a, b, c, d, e, f, g, h];
        for (acc, val) in self.state.iter_mut().zip(vals) {
            *acc = acc.wrapping_add(val);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        let empty = Sha256::new().finalize();
        assert_eq!(
            hex(&empty),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let mut abc = Sha256::new();
        abc.update(b"abc");
        assert_eq!(
            hex(&abc.finalize()),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let mut multi = Sha256::new();
        multi.update(b"abcdbcdecdefdefgefghfghighijhijk");
        multi.update(b"ijkljklmklmnlmnomnopnopq");
        assert_eq!(
            hex(&multi.finalize()),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn rejects_invalid_arguments() {
        assert!(validate(&json!({"path": ""})).is_err());
        assert!(validate(&json!({"path": "out", "max_entries": 0})).is_err());
        assert!(validate(&json!({"path": "out", "extra": true})).is_err());
        assert!(validate(&json!({"path": "out"})).is_ok());
    }

    #[test]
    fn max_entries_stops_reading_directory_after_the_cap() {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "da-fs-cap-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the cap fixture must be creatable");
        for index in 0..20 {
            std::fs::write(dir.join(format!("entry-{index:02}.txt")), "x")
                .expect("the cap fixture file must be writable");
        }

        let mut entries_seen = 0;
        let mut next_calls = 0;
        let entries = std::fs::read_dir(&dir)
            .expect("the cap fixture must be readable")
            .inspect(|_| {
                next_calls += 1;
            });
        let result = collect_directory_entries(entries, &mut entries_seen, 3, &dir, &dir);
        assert!(result.is_err(), "a directory over the cap must fail");
        assert_eq!(
            entries_seen, 4,
            "the cap must count the entry that exceeds it"
        );
        assert_eq!(
            next_calls, 4,
            "enumeration must stop at the cap instead of reading every entry"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
