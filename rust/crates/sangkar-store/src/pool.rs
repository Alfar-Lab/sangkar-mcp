//! One reader per repository root, keyed by canonical path (`DATABASE.md` §4, `TECH-SPEC.md` §6).
//!
//! The daemon serves several repositories from one process, and each root has its own `graph.db`
//! under its own `.sangkar` data directory. A handle is expensive to open and must not be rebuilt
//! per call, so an open one is kept here — and because a handle that is never used must not pin a
//! file (or a generation) forever, an idle one is dropped on [`ReaderPool::evict_idle`].
//!
//! **The pool owns no timer.** `evict_idle(now, idle_for)` is an explicit entry point: the caller
//! decides when to tick, which keeps the rule testable with an `Instant` the test names, and means
//! the pool never has to shut a thread down. Dropping an entry drops the last `Rc` on its reader,
//! which closes the connection; a handle a request still holds keeps serving the generation it
//! opened (`DATABASE.md` §5), and the next call for that root opens a fresh one.
//!
//! The key is [`canonical_root`]: `~/x`, `./x`, `x/../x` and a symlinked path all reach one entry,
//! so a caller cannot make the pool hold two handles on one file by spelling the root differently.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use sangkar_core::config::DATA_DIR_NAME;
use sangkar_core::errors::AppError;
use sangkar_core::registry::canonical_root;

use crate::db::open_reader;
use crate::read::reader::SqliteReader;

/// The file a publish writes and every reader opens (`DATABASE.md` §4).
pub const DATABASE_FILE: &str = "graph.db";

/// One handle per repository root, with the last time it was handed out.
///
/// Not `Send`/`Sync`: [`SqliteReader`] owns a [`crate::db::Conn`], which is a single-threaded
/// handle. The daemon keeps its pool on the thread that serves calls.
pub struct ReaderPool {
    roots: BTreeMap<PathBuf, PooledReader>,
}

struct PooledReader {
    reader: Rc<SqliteReader>,
    last_used: Instant,
}

impl ReaderPool {
    #[must_use]
    pub fn new() -> Self {
        Self {
            roots: BTreeMap::new(),
        }
    }

    /// The reader for `root`, opening it the first time and reusing it afterwards.
    ///
    /// The handle is `Rc`, so handing it out twice hands out the same reader — and the caller can
    /// coerce it to `Rc<dyn GraphReader>` for a [`crate::read::context::ToolContext`].
    ///
    /// # Errors
    ///
    /// Whatever [`crate::db::open_reader`] raises: a root with no published index fails here rather
    /// than opening an empty database, and nothing is cached on failure, so the next call retries.
    pub fn reader(&mut self, root: &Path) -> Result<Rc<SqliteReader>, AppError> {
        let key = canonical_root(root, None);
        if let Some(entry) = self.roots.get_mut(&key) {
            entry.last_used = Instant::now();
            return Ok(Rc::clone(&entry.reader));
        }
        let path = key.join(DATA_DIR_NAME).join(DATABASE_FILE);
        let reader = Rc::new(SqliteReader::new(open_reader(path)?));
        self.roots.insert(
            key,
            PooledReader {
                reader: Rc::clone(&reader),
                last_used: Instant::now(),
            },
        );
        Ok(reader)
    }

    /// Drops every handle not handed out for at least `idle_for`, and returns how many went.
    ///
    /// `now` is the caller's, so a test states the tick instead of sleeping. A handle still held by
    /// a caller stays open: this drops the pool's `Rc`, and the connection closes with the last one.
    pub fn evict_idle(&mut self, now: Instant, idle_for: Duration) -> usize {
        let before = self.roots.len();
        self.roots
            .retain(|_, entry| now.saturating_duration_since(entry.last_used) < idle_for);
        before - self.roots.len()
    }

    /// How many roots currently hold a handle.
    #[must_use]
    pub fn len(&self) -> usize {
        self.roots.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// Whether `root` already holds a handle, keyed the way [`Self::reader`] keys it.
    #[must_use]
    pub fn contains(&self, root: &Path) -> bool {
        self.roots.contains_key(&canonical_root(root, None))
    }
}

impl Default for ReaderPool {
    fn default() -> Self {
        Self::new()
    }
}
