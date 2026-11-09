//! The build cache behind `--cache`: the MVP of the incremental build
//! (ROADMAP §"Next cycle", item 4).
//!
//! **What this is.** A change detector with the last artifact attached. It records the
//! content hash of every page, the build options that fingerprint them, and the bytes
//! the last build produced. A rebuild that finds the same hashes returns those bytes
//! instead of re-reading and re-parsing the site; a rebuild that finds one different
//! hash does a full build and refreshes the record.
//!
//! **What this is not.** It does not merge postings. The first version of this module
//! cached the analyzed documents of every page and spliced them into the postings, and
//! on the 10k corpus that was *slower* than a cold build (1,781 ms against 1,299 ms):
//! the flat format assigns document ids by position, so one changed page moves every
//! id after it and the whole artifact is re-serialized anyway, while the page table had
//! to be reconstructed and a 109 MB cache rewritten. A cache that is slower on the case
//! it exists for is not a cache. Merging is the chunk-level rewrite with stable
//! document ids that the roadmap lists as the next step, and it is not pretended here.
//!
//! **Why it is still worth shipping.** The case that actually wastes CI and reviewer
//! time is a rebuild where *nothing* changed — a preview of a PR that touched a README, a
//! deploy triggered twice, a site generator that reruns. That build is now a hash pass
//! over the corpus plus one file read (measured at 38% of the cold build on 10k pages),
//! and the artifact is the one already on disk.
//!
//! The cache cannot change what a build produces: it stores hashes of the exact bytes
//! it read and returns the artifact of the build those hashes produced. Any mismatch —
//! a page, an option, a truncated file — is a miss, never a repair.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::BuildStats;

/// Marker + version. Another version is a miss, not an error.
const MAGIC: &[u8; 8] = b"RSCACHE3";
const FILE_NAME: &str = "build-cache.bin";

/// Largest artifact the cache keeps. Above this the hash table is still written (a
/// rebuild can then skip parsing but has to re-serialize), and the bytes are not
/// duplicated: a 700 MB artifact on disk is already the artifact.
const MAX_CACHED_ARTIFACT: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct CachedStats {
    pub documents: usize,
    pub terms: usize,
    pub tokens: u64,
    pub title_tokens: u64,
}

/// One output directory's build record.
#[derive(Debug, Clone, Default)]
pub struct BuildCache {
    fingerprint: u64,
    pages: HashMap<String, u64>,
    artifact: Option<Vec<u8>>,
    stats: CachedStats,
}

impl BuildCache {
    pub fn new(fingerprint: u64) -> Self {
        Self {
            fingerprint,
            ..Self::default()
        }
    }

    /// Load `dir/build-cache.bin` for `fingerprint`. Unreadable, truncated or foreign
    /// files are an empty cache: a build never fails because of its cache.
    pub fn load(dir: &Path, fingerprint: u64) -> Self {
        let Ok(bytes) = fs::read(dir.join(FILE_NAME)) else {
            return Self::new(fingerprint);
        };
        match decode(&bytes) {
            Some((stored, pages, artifact, stats)) if stored == fingerprint => Self {
                fingerprint,
                pages,
                artifact,
                stats,
            },
            _ => Self::new(fingerprint),
        }
    }

    pub fn fingerprint(&self) -> u64 {
        self.fingerprint
    }

    /// True when this record describes exactly the site in `hashes`: the same pages
    /// with the same bytes, and no page that is no longer there.
    pub fn covers(&self, hashes: &[(String, u64)]) -> bool {
        self.pages.len() == hashes.len()
            && hashes
                .iter()
                .all(|(path, hash)| self.pages.get(path) == Some(hash))
    }

    pub fn artifact(&self) -> Option<&[u8]> {
        self.artifact.as_deref()
    }

    pub fn stats(&self) -> CachedStats {
        self.stats
    }

    /// Record the site this build just wrote and its artifact, so the next rebuild
    /// with the same pages can answer with these bytes.
    pub fn record(&mut self, hashes: &[(String, u64)], bytes: &[u8], stats: &BuildStats) {
        self.pages = hashes.iter().cloned().collect();
        self.stats = CachedStats {
            documents: stats.documents,
            terms: stats.terms,
            tokens: stats.tokens,
            title_tokens: stats.title_tokens,
        };
        self.artifact = (bytes.len() <= MAX_CACHED_ARTIFACT).then(|| bytes.to_vec());
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        fs::create_dir_all(dir)?;
        let mut out = Vec::with_capacity(64 * 1024);
        out.extend_from_slice(MAGIC);
        put_u64(&mut out, self.fingerprint);
        put_u32(
            &mut out,
            u32::try_from(self.pages.len()).unwrap_or(u32::MAX),
        );
        // Sorted, so two builds of one site write the same file and a cache directory
        // can be diffed like the artifact.
        let mut paths: Vec<&String> = self.pages.keys().collect();
        paths.sort();
        for path in paths {
            let Some(hash) = self.pages.get(path) else {
                continue;
            };
            put_str(&mut out, path);
            put_u64(&mut out, *hash);
        }
        put_u32(
            &mut out,
            u32::try_from(self.stats.documents).unwrap_or(u32::MAX),
        );
        put_u32(
            &mut out,
            u32::try_from(self.stats.terms).unwrap_or(u32::MAX),
        );
        put_u64(&mut out, self.stats.tokens);
        put_u64(&mut out, self.stats.title_tokens);
        match &self.artifact {
            Some(bytes) => {
                put_u32(&mut out, u32::try_from(bytes.len()).unwrap_or(0));
                out.extend_from_slice(bytes);
            }
            None => put_u32(&mut out, 0),
        }
        // Write beside the destination and rename: an interrupted build leaves the
        // previous cache whole instead of a half file that decodes to something
        // plausible.
        let temporary = dir.join(format!("{FILE_NAME}.tmp"));
        fs::write(&temporary, &out)?;
        fs::rename(&temporary, dir.join(FILE_NAME))
    }
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_u32(out, u32::try_from(value.len()).unwrap_or(u32::MAX));
    out.extend_from_slice(value.as_bytes());
}

/// A bounds-checked reader: every access is an `Option`, because the cache file is as
/// untrusted as any other byte string this crate parses.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(len)?;
        let slice = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes([
            *bytes.first()?,
            *bytes.get(1)?,
            *bytes.get(2)?,
            *bytes.get(3)?,
        ]))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.take(8)?;
        let mut out = [0u8; 8];
        for (slot, byte) in out.iter_mut().zip(bytes.iter()) {
            *slot = *byte;
        }
        Some(u64::from_le_bytes(out))
    }

    fn string(&mut self) -> Option<String> {
        let len = self.u32()? as usize;
        String::from_utf8(self.take(len)?.to_vec()).ok()
    }
}

type Decoded = (u64, HashMap<String, u64>, Option<Vec<u8>>, CachedStats);

fn decode(bytes: &[u8]) -> Option<Decoded> {
    if bytes.get(..MAGIC.len())? != MAGIC {
        return None;
    }
    let mut reader = Reader::new(bytes);
    reader.take(MAGIC.len())?;
    let fingerprint = reader.u64()?;
    let count = reader.u32()? as usize;
    // A path is at least 9 bytes on disk (length + one char + hash); refusing an
    // absurd count before allocating keeps a corrupt file from asking for gigabytes.
    if count > bytes.len() / 9 + 1 {
        return None;
    }
    let mut pages = HashMap::with_capacity(count);
    for _ in 0..count {
        let path = reader.string()?;
        pages.insert(path, reader.u64()?);
    }
    let stats = CachedStats {
        documents: reader.u32()? as usize,
        terms: reader.u32()? as usize,
        tokens: reader.u64()?,
        title_tokens: reader.u64()?,
    };
    let artifact_len = reader.u32()? as usize;
    let artifact = if artifact_len == 0 {
        None
    } else {
        Some(reader.take(artifact_len)?.to_vec())
    };
    Some((fingerprint, pages, artifact, stats))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats() -> BuildStats {
        BuildStats {
            documents: 2,
            terms: 3,
            tokens: 10,
            title_tokens: 2,
            bytes: 8,
            ..BuildStats::default()
        }
    }

    fn dir(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("rustsearch-cache-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_saved_cache_reads_back_equal() {
        let dir = dir("roundtrip");
        let hashes = vec![("a.html".to_string(), 1u64), ("b/c.html".to_string(), 2u64)];
        let mut cache = BuildCache::new(42);
        cache.record(&hashes, b"artifact", &stats());
        cache.save(&dir).unwrap();
        let loaded = BuildCache::load(&dir, 42);
        assert!(loaded.covers(&hashes));
        assert_eq!(loaded.artifact(), Some(&b"artifact"[..]));
        assert_eq!(loaded.stats().documents, 2);
        // Another fingerprint is another cache: the pages are not trusted.
        assert!(!BuildCache::load(&dir, 43).covers(&hashes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_page_that_is_gone_or_changed_is_a_miss() {
        let dir = dir("mismatch");
        let hashes = vec![("a.html".to_string(), 1u64), ("b.html".to_string(), 2u64)];
        let mut cache = BuildCache::new(7);
        cache.record(&hashes, b"artifact", &stats());
        cache.save(&dir).unwrap();
        let loaded = BuildCache::load(&dir, 7);
        assert!(loaded.covers(&hashes));
        assert!(
            !loaded.covers(&hashes[..1]),
            "a deleted page is not covered"
        );
        let mut changed = hashes.clone();
        changed[1].1 = 3;
        assert!(!loaded.covers(&changed), "a changed page is not covered");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_truncated_or_foreign_file_is_a_miss_not_a_panic() {
        let dir = dir("bad");
        let hashes = vec![("a.html".to_string(), 1u64)];
        let mut cache = BuildCache::new(1);
        cache.record(&hashes, b"artifact", &stats());
        cache.save(&dir).unwrap();
        let full = fs::read(dir.join(FILE_NAME)).unwrap();
        for cut in [0, 4, 8, 9, 20, full.len() / 2, full.len() - 1] {
            fs::write(dir.join(FILE_NAME), &full[..cut]).unwrap();
            assert!(!BuildCache::load(&dir, 1).covers(&hashes), "cut {cut}");
        }
        fs::write(dir.join(FILE_NAME), b"not a cache at all").unwrap();
        assert!(!BuildCache::load(&dir, 1).covers(&hashes));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_artifact_above_the_ceiling_is_not_duplicated() {
        let dir = dir("large");
        let hashes = vec![("a.html".to_string(), 1u64)];
        let mut cache = BuildCache::new(1);
        cache.record(&hashes, &vec![0u8; MAX_CACHED_ARTIFACT + 1], &stats());
        cache.save(&dir).unwrap();
        let loaded = BuildCache::load(&dir, 1);
        assert!(loaded.covers(&hashes), "the page table is still written");
        assert!(loaded.artifact().is_none(), "the bytes are not duplicated");
        let _ = fs::remove_dir_all(&dir);
    }
}
