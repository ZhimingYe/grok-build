//! SQLite-backed memory index with FTS5 keyword search and optional sqlite-vec KNN.
//!
//! The index stores chunked text from memory files, with:
//! - A `chunks` table for structured metadata
//! - A contentless FTS5 virtual table for BM25 keyword search
//! - An optional `chunks_vec` vec0 table for vector similarity (when sqlite-vec is available)
//!
//! ## sqlite-vec Initialization
//!
//! Call [`init_sqlite_vec()`] once before creating any `MemoryIndex`.
//! This registers the sqlite-vec extension globally via `std::sync::Once`.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Once;

use rusqlite::{OptionalExtension as _, params};
use xai_sqlite_journal::JournalMode;

use super::chunker::{chunk_hash, chunk_markdown};
use super::schema;
use super::storage::MemoryStorage;
use xai_grok_config_types::MemoryIndexConfig;

static SQLITE_VEC_INIT: Once = Once::new();

/// Register the sqlite-vec extension globally. Call it before any `MemoryIndex::open_or_create()`; repeat calls are safe (Once guard).
pub fn init_sqlite_vec() {
    SQLITE_VEC_INIT.call_once(|| {
        // SAFETY: sqlite_vec::sqlite3_vec_init has the C ABI signature expected by sqlite3_auto_extension The explicit type
        // annotation on transmute makes this compiler-verified If sqlite-vec changes its init signature, the annotation will cause
        // a compile error instead of silent UB We pin sqlite-vec to exact version =0.1.7-alpha.2; any bump must re-verify.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::ffi::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> i32,
            >(
                sqlite_vec::sqlite3_vec_init as *const (),
            )));
        }
    });
}

/// A record from the `chunks` table.
#[derive(Debug, Clone)]
pub struct ChunkRecord {
    pub rowid: i64,
    pub id: String,
    pub path: String,
    pub start_line: usize,
    pub end_line: usize,
    pub text: String,
    pub hash: String,
    pub source: String,
    pub access_count: i64,
    pub created_at: i64,
}

/// Result of a FTS5 keyword search.
#[derive(Debug, Clone)]
pub struct FtsResult {
    pub chunk_id: String,
    pub rowid: i64,
    pub rank: f64,
}

/// Result of reindexing a file.
#[derive(Debug, Clone, Copy, Default)]
pub struct ReindexResult {
    pub added: usize,
    pub updated: usize,
    pub removed: usize,
}

/// SQLite-backed memory index.
pub struct MemoryIndex {
    db: rusqlite::Connection,
    #[expect(dead_code, reason = "used by later PRs for reindex_all / file reads")]
    storage: MemoryStorage,
    chunk_config: MemoryIndexConfig,
    /// Whether sqlite-vec loaded successfully (FTS always available).
    vec_available: bool,
    embedding_dimensions: usize,
}

impl MemoryIndex {
    /// Open or create the index database at `db_path`.
    /// `dimensions` sets the embedding vector size for the `chunks_vec` table.
    /// If sqlite-vec failed to load (call `init_sqlite_vec()` first), the index gracefully degrades to FTS-only mode.
    pub fn open_or_create(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // The mode decision statfs's the parent dir created above.
        Self::open_or_create_with_journal_mode(
            db_path,
            storage,
            config,
            dimensions,
            JournalMode::for_db_path(db_path),
        )
    }

    /// Open an index for lexical-only maintenance without changing the vector
    /// dimension already recorded by the embedding backend.
    // ZMY SELF-ADDED CODE START
    pub fn open_or_create_preserving_dimensions(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        fallback_dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        // ZMY SELF-ADDED CODE END
        if let Some(parent) = db_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let journal_mode = JournalMode::for_db_path(db_path);
        let db = journal_mode.open(db_path)?;
        let has_meta = db.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let stored_dimensions = if has_meta {
            db.query_row(
                schema::GET_META_SQL,
                params!["embedding_dimensions"],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        } else {
            None
        };
        // ZMY SELF-ADDED CODE START
        let fallback_dimensions = fallback_dimensions.max(1);
        let dimensions = stored_dimensions
            .as_deref()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|dimensions| *dimensions > 0);
        let has_vec_table = db.query_row(
            "SELECT EXISTS(
                SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'chunks_vec'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if (stored_dimensions.is_some() && dimensions.is_none())
            || (stored_dimensions.is_none() && has_vec_table)
        {
            return Self::initialize_fts_only(db, storage, config, fallback_dimensions);
        }
        let dimensions = dimensions.unwrap_or(fallback_dimensions);
        // ZMY SELF-ADDED CODE END
        Self::initialize(db, storage, config, dimensions)
    }

    // ZMY SELF-ADDED CODE START
    fn initialize_fts_only(
        db: rusqlite::Connection,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        db.execute_batch(&schema::schema_sql(dimensions, false))?;
        Ok(Self {
            db,
            storage,
            chunk_config: config,
            vec_available: false,
            embedding_dimensions: dimensions,
        })
    }
    // ZMY SELF-ADDED CODE END

    /// Open with an explicit journal mode. Tests use it to exercise the network-filesystem decision on a local disk.
    fn open_or_create_with_journal_mode(
        db_path: &Path,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        dimensions: usize,
        journal_mode: JournalMode,
    ) -> Result<Self, rusqlite::Error> {
        // busy_timeout + journal pragma live in the helper (see JournalMode::open).
        let db = journal_mode.open(db_path)?;
        Self::initialize(db, storage, config, dimensions)
    }

    fn initialize(
        db: rusqlite::Connection,
        storage: MemoryStorage,
        config: MemoryIndexConfig,
        dimensions: usize,
    ) -> Result<Self, rusqlite::Error> {
        // Check if sqlite-vec loaded (graceful fallback if not)
        // ZMY SELF-ADDED CODE START
        let mut vec_available =
            match db.query_row("SELECT vec_version()", [], |r| r.get::<_, String>(0)) {
                Ok(v) => {
                    tracing::info!(version = %v, "sqlite-vec loaded");
                    true
                }
                Err(_) => {
                    static WARNED: std::sync::Once = std::sync::Once::new();
                    WARNED.call_once(|| {
                        tracing::warn!("sqlite-vec not available, falling back to FTS-only search");
                    });
                    false
                }
            };
        // ZMY SELF-ADDED CODE END

        // ZMY SELF-ADDED CODE START
        db.execute_batch(&schema::schema_sql(dimensions, false))?;
        if vec_available {
            let initialize_vectors = || -> Result<(), rusqlite::Error> {
                let transaction = db.unchecked_transaction()?;
                let stored_dims: Option<String> = transaction
                    .query_row(schema::GET_META_SQL, params!["embedding_dimensions"], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if stored_dims
                    .as_ref()
                    .is_none_or(|stored| stored.parse::<usize>().ok() != Some(dimensions))
                {
                    // Missing dimension metadata cannot vouch for cached vectors.
                    transaction.execute("DROP TABLE IF EXISTS chunks_vec", [])?;
                }
                transaction.execute_batch(&schema::schema_sql(dimensions, true))?;
                Self::purge_invalidated_embeddings(&transaction)?;
                transaction.execute(
                    schema::UPSERT_META_SQL,
                    params!["embedding_dimensions", dimensions.to_string()],
                )?;
                transaction.commit()
            };
            if let Err(error) = initialize_vectors() {
                tracing::warn!(%error, "vector index unavailable; retaining full-text search");
                vec_available = false;
            }
        }
        // ZMY SELF-ADDED CODE END

        Ok(Self {
            db,
            storage,
            chunk_config: config,
            vec_available,
            embedding_dimensions: dimensions,
        })
    }

    /// Whether sqlite-vec is available for vector operations.
    pub fn vec_available(&self) -> bool {
        self.vec_available
    }

    /// Embedding dimensions configured for this index.
    pub fn embedding_dimensions(&self) -> usize {
        self.embedding_dimensions
    }

    /// Direct access to the underlying SQLite connection (test-only).
    #[cfg(test)]
    pub(crate) fn db(&self) -> &rusqlite::Connection {
        &self.db
    }

    // -----------------------------------------------------------------------
    // Indexing
    // -----------------------------------------------------------------------

    /// Reindex a single memory file. Compares chunk hashes to avoid redundant work.
    ///
    /// `source` should be `"global"`, `"workspace"`, or `"session"`.
    pub fn reindex_file(
        &mut self,
        path: &Path,
        source: &str,
    ) -> Result<ReindexResult, rusqlite::Error> {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read file for reindexing");
                return Ok(ReindexResult::default());
            }
        };
        self.reindex_content(path, source, &content)
    }

    /// Reindex a path from bytes already read and validated by the caller.
    ///
    /// This avoids a second filesystem read when the caller must bind indexed
    /// content to a durable hash.
    pub fn reindex_content(
        &mut self,
        path: &Path,
        source: &str,
        content: &str,
    ) -> Result<ReindexResult, rusqlite::Error> {
        let span = tracing::info_span!(
            "memory.reindex_file",
            bytes = tracing::field::Empty,
            chunk_count = tracing::field::Empty,
        );
        let _g = span.enter();
        span.record("bytes", content.len() as i64);

        let new_chunks = chunk_markdown(content, &self.chunk_config);
        span.record("chunk_count", new_chunks.len() as i64);
        let path_str = path.to_string_lossy().to_string();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut result = ReindexResult::default();
        let mut seen_ids = std::collections::HashSet::new();

        // Keep chunks, FTS, and vec mutations in one transaction. IMMEDIATE locks before
        // reading existing chunks so concurrent writers cannot both observe an empty path
        // and race to insert the same chunk ids.
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = Self::get_chunks_for_path(&tx, &path_str)?;

        for (i, chunk) in new_chunks.iter().enumerate() {
            let chunk_id = format!("{}:{}", path_str, i);
            let hash = chunk_hash(&chunk.text);
            seen_ids.insert(chunk_id.clone());

            match existing.get(&chunk_id) {
                Some(old) if old.hash == hash => {
                    // Unchanged, skip
                }
                Some(old) => {
                    // Changed: update chunk, delete stale FTS entry, insert new one
                    tx.execute(
                        "UPDATE chunks SET text = ?1, hash = ?2, start_line = ?3, \
                         end_line = ?4, updated_at = ?5 WHERE id = ?6",
                        params![
                            chunk.text,
                            hash,
                            chunk.start_line,
                            chunk.end_line,
                            now,
                            chunk_id
                        ],
                    )?;
                    // Delete old FTS entry and insert new one
                    tx.execute(
                        "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                        params![old.rowid, old.text],
                    )?;
                    let new_rowid: Option<i64> = tx
                        .query_row(
                            "SELECT rowid FROM chunks WHERE id = ?1",
                            params![chunk_id],
                            |row| row.get(0),
                        )
                        .ok();
                    if let Some(rid) = new_rowid {
                        tx.execute(
                            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                            params![rid, chunk.text],
                        )?;
                    }
                    // ZMY SELF-ADDED CODE START
                    // Persist tombstones so failed vector deletes can be retried after recovery.
                    Self::invalidate_embedding(&tx, &chunk_id, self.vec_available)?;
                    // ZMY SELF-ADDED CODE END
                    result.updated += 1;
                }
                None => {
                    // New chunk
                    tx.execute(
                        "INSERT INTO chunks (id, path, start_line, end_line, text, hash, source, \
                         created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                        params![
                            chunk_id,
                            path_str,
                            chunk.start_line,
                            chunk.end_line,
                            chunk.text,
                            hash,
                            source,
                            now,
                            now,
                        ],
                    )?;
                    let rowid = tx.last_insert_rowid();
                    tx.execute(
                        "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                        params![rowid, chunk.text],
                    )?;
                    result.added += 1;
                }
            }
        }

        // Delete chunks no longer in file
        for (old_id, old_record) in &existing {
            if !seen_ids.contains(old_id) {
                tx.execute(
                    "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                    params![old_record.rowid, old_record.text],
                )?;
                // ZMY SELF-ADDED CODE START
                // Persist tombstones so failed vector deletes can be retried after recovery.
                Self::invalidate_embedding(&tx, old_id, self.vec_available)?;
                // ZMY SELF-ADDED CODE END
                tx.execute("DELETE FROM chunks WHERE id = ?1", params![old_id])?;
                result.removed += 1;
            }
        }

        tx.commit()?;
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Search
    // -----------------------------------------------------------------------

    // ZMY SELF-ADDED CODE START
    /// FTS5 keyword search with a bounded literal fallback for non-tokenized text.
    // Merge substring matches so FTS hits cannot hide valid literal matches.
    pub fn search_fts_by_sources(
        &self,
        query: &str,
        limit: usize,
        sources: &[&str],
    ) -> Result<Vec<FtsResult>, rusqlite::Error> {
        if sources.is_empty() || limit == 0 {
            return Ok(vec![]);
        }
        let fts_query = super::query_expansion::lexical_search_query(query);
        let fts_results = if !fts_query.is_empty() {
            let _g = tracing::info_span!("memory.fts_search").entered();
            let placeholders: Vec<String> = sources
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 3))
                .collect();
            let sql = format!(
                "SELECT f.rowid, f.rank FROM chunks_fts f \
                 JOIN chunks c ON f.rowid = c.rowid \
                 WHERE chunks_fts MATCH ?1 AND c.source IN ({}) \
                 ORDER BY f.rank LIMIT ?2",
                placeholders.join(", ")
            );
            let mut stmt = self.db.prepare(&sql)?;
            let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
            params_vec.push(Box::new(fts_query));
            params_vec.push(Box::new(limit as i64));
            for source in sources {
                params_vec.push(Box::new(source.to_string()));
            }
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params_vec.iter().map(|param| param.as_ref()).collect();
            let rows = stmt
                .query_map(param_refs.as_slice(), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            self.resolve_fts_rowids(rows)?
        } else {
            vec![]
        };
        if fts_results.is_empty() || Self::needs_literal_fallback(query) {
            let literal_rank = Self::literal_rank(&fts_results);
            let literal_results =
                self.search_fts_substring(query, limit, Some(sources), literal_rank)?;
            if !literal_results.is_empty() {
                return Ok(Self::merge_fts_results(literal_results, fts_results, limit));
            }
        }
        Ok(fts_results)
    }

    pub fn search_fts(&self, query: &str, limit: usize) -> Result<Vec<FtsResult>, rusqlite::Error> {
        if limit == 0 {
            return Ok(vec![]);
        }

        let fts_query = super::query_expansion::lexical_search_query(query);
        let fts_results = if !fts_query.is_empty() {
            let _g = tracing::info_span!("memory.fts_search").entered();
            let mut stmt = self.db.prepare(
                "SELECT rowid, rank FROM chunks_fts WHERE chunks_fts MATCH ?1 \
                 ORDER BY rank LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(params![fts_query, limit], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            self.resolve_fts_rowids(rows)?
        } else {
            vec![]
        };
        if fts_results.is_empty() || Self::needs_literal_fallback(query) {
            let literal_rank = Self::literal_rank(&fts_results);
            let literal_results = self.search_fts_substring(query, limit, None, literal_rank)?;
            if !literal_results.is_empty() {
                return Ok(Self::merge_fts_results(literal_results, fts_results, limit));
            }
        }
        Ok(fts_results)
    }

    fn needs_literal_fallback(query: &str) -> bool {
        query.chars().any(|character| {
            (!character.is_ascii() && character.is_alphanumeric())
                || character.is_ascii_punctuation()
        })
    }

    fn literal_rank(fts_results: &[FtsResult]) -> f64 {
        fts_results
            .iter()
            .map(|result| result.rank)
            .min_by(f64::total_cmp)
            .map_or(-1.0e-6, |rank| rank - 1.0)
    }

    fn merge_fts_results(
        literal_results: Vec<FtsResult>,
        fts_results: Vec<FtsResult>,
        limit: usize,
    ) -> Vec<FtsResult> {
        let mut seen = std::collections::HashSet::with_capacity(limit);
        let mut merged = Vec::with_capacity(limit.min(literal_results.len() + fts_results.len()));
        for result in literal_results.into_iter().chain(fts_results) {
            if seen.insert(result.chunk_id.clone()) {
                merged.push(result);
                if merged.len() == limit {
                    break;
                }
            }
        }
        merged
    }

    fn search_fts_substring(
        &self,
        query: &str,
        limit: usize,
        sources: Option<&[&str]>,
        rank: f64,
    ) -> Result<Vec<FtsResult>, rusqlite::Error> {
        let valid_literal = |literal: &str| {
            let char_count = literal.chars().take(1025).count();
            char_count > 0
                && char_count <= 1024
                && literal.chars().any(char::is_alphanumeric)
                && (char_count > 1
                    || literal
                        .chars()
                        .any(|c| !c.is_ascii() && c.is_alphanumeric()))
        };
        let phrase = query.trim();
        let has_phrase = valid_literal(phrase);
        let mut literals = Vec::new();
        let mut seen = std::collections::HashSet::new();
        if has_phrase {
            literals.push(phrase);
            seen.insert(phrase.to_lowercase());
        }
        // Whitespace/list separators delimit keywords, not technical symbols such as C++.
        for term in query.split(|c: char| c.is_whitespace() || "，。；、,;".contains(c)) {
            let term = term.trim_matches(|c: char| "\"'()[]{}!?！？".contains(c));
            if valid_literal(term)
                && (Self::needs_literal_fallback(term) || term.chars().all(|c| c.is_ascii_digit()))
                && seen.insert(term.to_lowercase())
            {
                literals.push(term);
                if literals.len() >= 64 {
                    break;
                }
            }
        }
        if literals.is_empty() {
            return Ok(vec![]);
        }

        let predicates: Vec<String> = (1..=literals.len())
            .map(|i| format!("instr(lower(c.text), lower(?{i})) > 0"))
            .collect();
        let phrase_predicate = if has_phrase {
            "instr(lower(c.text), lower(?1)) > 0"
        } else {
            "0"
        };
        let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = literals
            .into_iter()
            .map(|literal| Box::new(literal.to_owned()) as Box<dyn rusqlite::types::ToSql>)
            .collect();
        params_vec.push(Box::new(limit as i64));
        let limit_parameter = params_vec.len();
        let mut source_filter = String::new();
        if let Some(sources) = sources {
            let mut placeholders = Vec::new();
            for source in sources {
                params_vec.push(Box::new(source.to_string()));
                placeholders.push(format!("?{}", params_vec.len()));
            }
            source_filter = format!(" AND c.source IN ({})", placeholders.join(", "));
        }
        let sql = format!(
            "SELECT c.rowid, ({phrase_predicate}) AS phrase_match FROM chunks c \
             WHERE ({}){source_filter} \
             ORDER BY phrase_match DESC, c.created_at DESC, c.path ASC, c.id ASC \
             LIMIT ?{limit_parameter}",
            predicates.join(" OR ")
        );
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params_vec.iter().map(|param| param.as_ref()).collect();
        let mut stmt = self.db.prepare(&sql)?;
        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                let phrase_match = row.get::<_, bool>(1)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    if phrase_match { rank - 1.0 } else { rank },
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        self.resolve_fts_rowids(rows)
    }
    // ZMY SELF-ADDED CODE END

    fn resolve_fts_rowids(&self, rows: Vec<(i64, f64)>) -> Result<Vec<FtsResult>, rusqlite::Error> {
        let mut results = Vec::with_capacity(rows.len());
        for (rowid, rank) in rows {
            if let Ok(chunk_id) = self.db.query_row(
                "SELECT id FROM chunks WHERE rowid = ?1",
                params![rowid],
                |row| row.get::<_, String>(0),
            ) {
                results.push(FtsResult {
                    chunk_id,
                    rowid,
                    rank,
                });
            }
        }
        Ok(results)
    }

    /// Get a chunk by its ID.
    pub fn get_chunk(&self, id: &str) -> Result<Option<ChunkRecord>, rusqlite::Error> {
        let mut stmt = self.db.prepare(
            "SELECT rowid, id, path, start_line, end_line, text, hash, source, access_count, \
             created_at FROM chunks WHERE id = ?1",
        )?;
        let result = stmt
            .query_row(params![id], |row| {
                Ok(ChunkRecord {
                    rowid: row.get(0)?,
                    id: row.get(1)?,
                    path: row.get(2)?,
                    start_line: row.get::<_, i64>(3)? as usize,
                    end_line: row.get::<_, i64>(4)? as usize,
                    text: row.get(5)?,
                    hash: row.get(6)?,
                    source: row.get(7)?,
                    access_count: row.get(8)?,
                    created_at: row.get(9)?,
                })
            })
            .ok();
        Ok(result)
    }

    /// An empty string means no claim is active.
    /// A non-empty claim means a session currently owns the reindex lock (or a crashed session left a stale one).
    /// `grok memory doctor` uses this to detect stuck states.
    pub fn get_reindex_claim(&self) -> String {
        self.db
            .query_row(
                "SELECT value FROM meta WHERE key = 'reindex_claim'",
                [],
                |r| r.get::<_, String>(0),
            )
            .unwrap_or_default()
    }

    /// Return all distinct file paths that have at least one indexed chunk.
    ///
    /// `grok memory doctor` uses this to detect orphaned chunks (chunks whose source file has since been deleted).
    pub fn all_indexed_paths(&self) -> Result<Vec<String>, rusqlite::Error> {
        let mut stmt = self
            .db
            .prepare("SELECT DISTINCT path FROM chunks ORDER BY path")?;
        let paths = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    /// Record an access to a chunk (increments access_count, updates last_accessed).
    pub fn record_access(&mut self, chunk_id: &str) -> Result<(), rusqlite::Error> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.db.execute(
            "UPDATE chunks SET access_count = access_count + 1, last_accessed = ?1 WHERE id = ?2",
            params![now, chunk_id],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Vector operations (no-op if !vec_available)
    // -----------------------------------------------------------------------

    /// Return chunks that don't have embeddings yet.
    pub fn chunks_without_embeddings(&self) -> Result<Vec<(String, String)>, rusqlite::Error> {
        if !self.vec_available {
            return Ok(vec![]);
        }
        // ZMY SELF-ADDED CODE START
        self.clear_invalidated_embeddings()?;
        // ZMY SELF-ADDED CODE END
        let mut stmt = self.db.prepare(
            "SELECT c.id, c.text FROM chunks c \
             LEFT JOIN chunks_vec_rowids v ON v.id = c.id \
             WHERE v.id IS NULL",
        )?;
        let results = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(results)
    }

    /// Insert or update an embedding for a chunk.
    pub fn upsert_embedding(
        &self,
        chunk_id: &str,
        embedding: &[f32],
    ) -> Result<(), rusqlite::Error> {
        if !self.vec_available {
            return Ok(());
        }
        // ZMY SELF-ADDED CODE START
        self.clear_invalidated_embeddings()?;
        // ZMY SELF-ADDED CODE END
        let embedding_bytes: Vec<u8> = embedding.iter().flat_map(|f| f.to_le_bytes()).collect();
        self.db.execute(
            "INSERT OR REPLACE INTO chunks_vec(chunk_id, embedding) VALUES (?1, ?2)",
            params![chunk_id, embedding_bytes],
        )?;
        Ok(())
    }

    /// KNN vector search. Returns (chunk_id, distance) pairs.
    pub fn vector_search(
        &self,
        query_embedding: &[f32],
        k: usize,
    ) -> Result<Vec<(String, f32)>, rusqlite::Error> {
        if !self.vec_available {
            return Ok(vec![]);
        }
        // ZMY SELF-ADDED CODE START
        self.clear_invalidated_embeddings()?;
        // ZMY SELF-ADDED CODE END
        let span = tracing::info_span!(
            "memory.vector_search",
            candidate_count = tracing::field::Empty
        );
        let _g = span.enter();
        let query_bytes: Vec<u8> = query_embedding
            .iter()
            .flat_map(|f| f.to_le_bytes())
            .collect();
        let mut stmt = self.db.prepare(
            "SELECT chunk_id, distance FROM chunks_vec \
             WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance",
        )?;
        let results = stmt
            .query_map(params![query_bytes, k], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, f32>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        span.record("candidate_count", results.len() as i64);
        Ok(results)
    }

    // -----------------------------------------------------------------------
    // Reindex claim coordination (multi-agent)
    // -----------------------------------------------------------------------

    /// Try to claim exclusive reindex rights using the `meta` table.
    /// Uses an atomic UPDATE: succeeds only if unclaimed (empty) or stale (older than `stale_threshold_secs`).
    /// Under SQLite's serialized writer model, at most one agent wins.
    pub fn try_claim_reindex(&self, stale_threshold_secs: i64) -> bool {
        let pid = std::process::id();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let claim_value = format!("{pid}:{now}");
        let stale_cutoff = now - stale_threshold_secs;

        let rows = self
            .db
            .execute(
                "UPDATE meta SET value = ?1 WHERE key = 'reindex_claim' \
             AND (value = '' OR CAST(SUBSTR(value, INSTR(value, ':') + 1) AS INTEGER) < ?2)",
                params![claim_value, stale_cutoff],
            )
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "reindex claim SQL failed, treating as claim-fail");
                0
            });

        rows == 1
    }

    /// Release the reindex claim. Call after reindex completes.
    pub fn release_claim(&self) {
        let _ = self
            .db
            .execute("UPDATE meta SET value = '' WHERE key = 'reindex_claim'", []);
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Delete all indexed chunks for a given file path.
    /// The watcher calls this on file-removal events so deleted memory files stop being searchable.
    /// Deletes from all three tables (`chunks`, `chunks_fts`, `chunks_vec`) in one transaction so the index stays consistent even on partial failure.
    pub fn delete_path(&mut self, path: &Path) -> Result<usize, rusqlite::Error> {
        let path_str = path.to_string_lossy().to_string();
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let existing = Self::get_chunks_for_path(&tx, &path_str)?;

        if existing.is_empty() {
            return Ok(0);
        }

        for (chunk_id, record) in &existing {
            // Contentless FTS5 requires the original text for the 'delete' command.
            tx.execute(
                "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
                params![record.rowid, record.text],
            )?;
            // ZMY SELF-ADDED CODE START
            // Persist tombstones so failed vector deletes can be retried after recovery.
            Self::invalidate_embedding(&tx, chunk_id, self.vec_available)?;
            // ZMY SELF-ADDED CODE END
            tx.execute("DELETE FROM chunks WHERE id = ?1", params![chunk_id])?;
        }
        tx.commit()?;

        tracing::debug!(
            path = %path.display(),
            count = existing.len(),
            "memory index: deleted chunks for removed file"
        );
        Ok(existing.len())
    }

    // ZMY SELF-ADDED CODE START
    fn invalidate_embedding(
        transaction: &rusqlite::Transaction<'_>,
        chunk_id: &str,
        vec_available: bool,
    ) -> Result<(), rusqlite::Error> {
        transaction.execute(
            "INSERT OR IGNORE INTO embedding_invalidations(chunk_id) VALUES (?1)",
            params![chunk_id],
        )?;
        if vec_available
            && transaction
                .execute(
                    "DELETE FROM chunks_vec WHERE chunk_id = ?1",
                    params![chunk_id],
                )
                .is_ok()
        {
            transaction.execute(
                "DELETE FROM embedding_invalidations WHERE chunk_id = ?1",
                params![chunk_id],
            )?;
        }
        Ok(())
    }

    fn purge_invalidated_embeddings(
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<(), rusqlite::Error> {
        let mut stmt = transaction.prepare("SELECT chunk_id FROM embedding_invalidations")?;
        let ids = stmt
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for id in ids {
            transaction.execute("DELETE FROM chunks_vec WHERE chunk_id = ?1", params![id])?;
        }
        transaction.execute("DELETE FROM embedding_invalidations", [])?;
        Ok(())
    }

    fn clear_invalidated_embeddings(&self) -> Result<(), rusqlite::Error> {
        let pending: bool = self.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM embedding_invalidations)",
            [],
            |row| row.get(0),
        )?;
        if pending {
            let transaction = self.db.unchecked_transaction()?;
            Self::purge_invalidated_embeddings(&transaction)?;
            transaction.commit()?;
        }
        Ok(())
    }
    // ZMY SELF-ADDED CODE END

    /// Get all existing chunks for a path, keyed by chunk ID.
    ///
    /// Takes the connection (or an open transaction) explicitly so mutating callers
    /// read the snapshot inside the same write transaction they mutate under.
    fn get_chunks_for_path(
        connection: &rusqlite::Connection,
        path: &str,
    ) -> Result<HashMap<String, ChunkRecord>, rusqlite::Error> {
        let mut stmt = connection.prepare(
            "SELECT rowid, id, path, start_line, end_line, text, hash, source, access_count, \
             created_at FROM chunks WHERE path = ?1",
        )?;
        let rows = stmt.query_map(params![path], |row| {
            Ok(ChunkRecord {
                rowid: row.get(0)?,
                id: row.get(1)?,
                path: row.get(2)?,
                start_line: row.get::<_, i64>(3)? as usize,
                end_line: row.get::<_, i64>(4)? as usize,
                text: row.get(5)?,
                hash: row.get(6)?,
                source: row.get(7)?,
                access_count: row.get(8)?,
                created_at: row.get(9)?,
            })
        })?;
        let mut map = HashMap::new();
        for row in rows {
            let record = row?;
            map.insert(record.id.clone(), record);
        }
        Ok(map)
    }

    /// Get the rowid for a chunk by its text ID.
    #[expect(
        dead_code,
        reason = "used by future reindex_all and non-transactional paths"
    )]
    fn get_rowid(&self, id: &str) -> Result<Option<i64>, rusqlite::Error> {
        self.db
            .query_row(
                "SELECT rowid FROM chunks WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .ok()
            .map_or(Ok(None), |v| Ok(Some(v)))
    }

    /// Insert an FTS5 entry for a chunk. The FTS table is contentless, so entries are managed by hand.
    #[expect(
        dead_code,
        reason = "used by future reindex_all and non-transactional paths"
    )]
    fn insert_fts_entry(&self, rowid: i64, text: &str) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            params![rowid, text],
        )?;
        Ok(())
    }

    /// Delete an FTS5 entry. For contentless FTS5, this requires the original text.
    #[expect(
        dead_code,
        reason = "used by future reindex_all and non-transactional paths"
    )]
    fn delete_fts_entry(&self, rowid: i64, text: &str) -> Result<(), rusqlite::Error> {
        self.db.execute(
            "INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES('delete', ?1, ?2)",
            params![rowid, text],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_storage(tmp: &TempDir) -> MemoryStorage {
        let global = tmp.path().join("memory");
        let workspace = global.join("test_ws");
        MemoryStorage::with_paths(global, workspace)
    }

    fn test_index(tmp: &TempDir) -> MemoryIndex {
        let db_path = tmp.path().join("test.sqlite");
        let storage = test_storage(tmp);
        MemoryIndex::open_or_create(&db_path, storage, MemoryIndexConfig::default(), 1536).unwrap()
    }

    #[test]
    fn test_open_or_create_reopens_existing() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage = test_storage(&tmp);

        // Create
        {
            let _idx = MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                MemoryIndexConfig::default(),
                1536,
            )
            .unwrap();
        }

        // Reopen
        let idx =
            MemoryIndex::open_or_create(&db_path, storage, MemoryIndexConfig::default(), 1536)
                .unwrap();
        assert_eq!(idx.embedding_dimensions(), 1536);
    }

    fn journal_mode(idx: &MemoryIndex) -> String {
        idx.db()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap()
    }

    /// Two connections indexing the same not-yet-indexed file must serialize on the write
    /// lock rather than both inserting the same chunk ids (`UNIQUE constraint failed: chunks.id`).
    #[test]
    fn concurrent_reindex_of_same_file_across_connections_is_safe() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let file = tmp.path().join("note.md");
        std::fs::write(&file, "# Note\n\nShared chunk one.\n\nShared chunk two.\n").unwrap();
        let workers = 6;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                let db_path = db_path.clone();
                let file = file.clone();
                let storage = test_storage(&tmp);
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let mut idx = MemoryIndex::open_or_create(
                        &db_path,
                        storage,
                        MemoryIndexConfig::default(),
                        1536,
                    )
                    .unwrap();
                    barrier.wait();
                    idx.reindex_file(&file, "workspace").unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        let idx = MemoryIndex::open_or_create(
            &db_path,
            test_storage(&tmp),
            MemoryIndexConfig::default(),
            1536,
        )
        .unwrap();
        let chunks: i64 = idx
            .db()
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE path = ?1",
                params![file.to_string_lossy()],
                |r| r.get(0),
            )
            .unwrap();
        let fts: i64 = idx
            .db()
            .query_row("SELECT COUNT(*) FROM chunks_fts", [], |r| r.get(0))
            .unwrap();
        assert!(chunks >= 1);
        assert_eq!(fts, chunks);
    }

    #[test]
    fn test_open_or_create_uses_wal_on_local_fs() {
        // GROK_SQLITE_JOURNAL_MODE overrides the journal-mode decision; skip when it is set
        if std::env::var("GROK_SQLITE_JOURNAL_MODE").is_ok() {
            return;
        }
        let tmp = TempDir::new().unwrap();
        assert_eq!(journal_mode(&test_index(&tmp)), "wal");
    }

    #[test]
    fn test_network_mode_uses_fresh_per_host_truncate_db() {
        // Network mode opens a per-host sibling of the given path in rollback-journal mode
        // The legacy shared file is left untouched; a live old binary can flip it back to WAL at any time
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage = test_storage(&tmp);

        let idx = MemoryIndex::open_or_create_with_journal_mode(
            &db_path,
            storage,
            MemoryIndexConfig::default(),
            512,
            JournalMode::Truncate,
        )
        .unwrap();
        assert_eq!(journal_mode(&idx), "truncate");
        assert_eq!(idx.embedding_dimensions(), 512);
        drop(idx);

        let eff = JournalMode::Truncate.effective_db_path(&db_path);
        assert_ne!(eff, db_path);
        assert!(eff.exists());
        let base = eff.display().to_string();
        assert!(!std::fs::exists(format!("{base}-wal")).unwrap());
        assert!(!std::fs::exists(format!("{base}-shm")).unwrap());
    }

    #[test]
    fn test_reindex_file_adds_chunks() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        // Write a test memory file
        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Title\n\nSome content here.").unwrap();

        let result = idx.reindex_file(&file_path, "workspace").unwrap();
        assert_eq!(result.added, 1);
        assert_eq!(result.updated, 0);
        assert_eq!(result.removed, 0);
    }

    #[test]
    fn reindex_content_uses_the_callers_verified_bytes() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);
        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Changed\n\nUnverified disk content.").unwrap();

        idx.reindex_content(
            &file_path,
            "workspace",
            "# Verified\n\nHash-bound durable content.",
        )
        .unwrap();

        assert!(!idx.search_fts("hash bound durable", 10).unwrap().is_empty());
        assert!(idx.search_fts("unverified disk", 10).unwrap().is_empty());
    }

    #[test]
    fn test_reindex_file_detects_changes() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Title\n\nOriginal content.").unwrap();
        let r1 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert_eq!(r1.added, 1);

        // Modify file
        std::fs::write(&file_path, "# Title\n\nUpdated content.").unwrap();
        let r2 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert_eq!(r2.updated, 1);
        assert_eq!(r2.added, 0);
    }

    #[test]
    fn test_reindex_file_removes_deleted_chunks() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        // Write file with content that produces at least 2 chunks
        let big = format!(
            "## Section 1\n\n{}\n\n## Section 2\n\n{}",
            "A".repeat(2000),
            "B".repeat(2000)
        );
        std::fs::write(&file_path, &big).unwrap();
        let r1 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert!(r1.added >= 2, "should add at least 2 chunks");

        // Shrink to 1 chunk
        std::fs::write(&file_path, "## Only Section\n\nSmall.").unwrap();
        let r2 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert!(r2.removed > 0, "should remove old chunks");
    }

    #[test]
    fn test_reindex_unchanged_is_noop() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Static\n\nContent.").unwrap();

        let r1 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert_eq!(r1.added, 1);

        let r2 = idx.reindex_file(&file_path, "workspace").unwrap();
        assert_eq!(r2.added, 0);
        assert_eq!(r2.updated, 0);
        assert_eq!(r2.removed, 0);
    }

    #[test]
    fn test_fts_search() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Guide\n\nRust programming language tutorial.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        let results = idx.search_fts("rust programming", 10).unwrap();
        assert!(!results.is_empty(), "FTS should find 'rust programming'");
    }

    #[test]
    fn test_fts_search_no_match() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Guide\n\nPython tutorial.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        let results = idx.search_fts("haskell monads", 10).unwrap();
        assert!(results.is_empty(), "FTS should not find unrelated query");
    }

    #[test]
    fn test_get_chunk_and_record_access() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("test.md");
        std::fs::write(&file_path, "# Test\n\nChunk content.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        let path_str = file_path.to_string_lossy().to_string();
        let chunk_id = format!("{path_str}:0");

        let chunk = idx.get_chunk(&chunk_id).unwrap();
        assert!(chunk.is_some());
        let chunk = chunk.unwrap();
        assert_eq!(chunk.access_count, 0);

        idx.record_access(&chunk_id).unwrap();
        let chunk = idx.get_chunk(&chunk_id).unwrap().unwrap();
        assert_eq!(chunk.access_count, 1);
    }

    #[test]
    fn test_sqlite_vec_loads_and_reports_version() {
        init_sqlite_vec();
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let version: String = conn
            .query_row("SELECT vec_version()", [], |r| r.get(0))
            .unwrap();
        assert!(!version.is_empty(), "sqlite-vec should report a version");
    }

    // -----------------------------------------------------------------------
    // Append-then-reindex regression test
    // -----------------------------------------------------------------------

    /// Simulates the `/memory append` then immediate-reindex flow.
    /// Appended content was then only searchable after a watcher-driven sync or the next session startup.
    #[test]
    fn test_append_then_reindex_is_immediately_searchable() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        // Simulate MemoryStorage::append_to_memory writing the file.
        let mem_file = tmp.path().join("workspace_memory.md");
        std::fs::write(
            &mem_file,
            "## Rust Tip\n\nAlways prefer references over clones.",
        )
        .unwrap();

        // The content is not yet in the index, so search returns nothing
        let pre = idx.search_fts("prefer references over clones", 10).unwrap();
        assert!(
            pre.is_empty(),
            "content must not be searchable before reindex_file is called"
        );

        // Immediate reindex (what reindex_appended_memory in app.rs does).
        idx.reindex_file(&mem_file, "workspace").unwrap();

        // Now the content must be searchable without watcher or restart.
        let post = idx.search_fts("prefer references over clones", 10).unwrap();
        assert!(
            !post.is_empty(),
            "appended content must be immediately searchable after reindex_file"
        );
    }

    // -----------------------------------------------------------------------
    // delete_path tests
    // -----------------------------------------------------------------------

    /// Deleting an indexed file removes all its chunks and they are no longer searchable.
    #[test]
    fn test_delete_path_removes_chunks_and_fts_entries() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("removeme.md");
        std::fs::write(&file_path, "# Rust Guide\n\nRust ownership rules.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        // Precondition: FTS finds the file.
        let pre = idx.search_fts("rust ownership", 10).unwrap();
        assert!(!pre.is_empty(), "should find content before deletion");

        // Delete the path from the index (simulating a watcher Remove event).
        let removed = idx.delete_path(&file_path).unwrap();
        assert!(removed >= 1, "should report at least 1 chunk removed");

        // Post-condition: FTS no longer returns the deleted content.
        let post = idx.search_fts("rust ownership", 10).unwrap();
        assert!(
            post.is_empty(),
            "deleted file must not be searchable after delete_path"
        );
    }

    /// delete_path is idempotent: calling it twice (or on an unindexed file) is safe.
    #[test]
    fn test_delete_path_idempotent() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file_path = tmp.path().join("idempotent.md");
        std::fs::write(&file_path, "# Entry\n\nSome content.").unwrap();
        idx.reindex_file(&file_path, "workspace").unwrap();

        let first = idx.delete_path(&file_path).unwrap();
        assert!(first >= 1);

        // Calling again on an already-deleted path returns 0 without error.
        let second = idx.delete_path(&file_path).unwrap();
        assert_eq!(second, 0, "second delete_path must be a no-op (idempotent)");
    }

    /// delete_path on a path that was never indexed returns 0.
    #[test]
    fn test_delete_path_never_indexed_returns_zero() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let phantom = tmp.path().join("phantom.md");
        let removed = idx.delete_path(&phantom).unwrap();
        assert_eq!(removed, 0, "unindexed path must return 0");
    }

    /// delete_path does not affect chunks from other files.
    #[test]
    fn test_delete_path_does_not_affect_other_files() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let keep = tmp.path().join("keep.md");
        let remove = tmp.path().join("remove.md");
        std::fs::write(&keep, "# Keep\n\nPython tutorial.").unwrap();
        std::fs::write(&remove, "# Remove\n\nRust tutorial.").unwrap();

        idx.reindex_file(&keep, "workspace").unwrap();
        idx.reindex_file(&remove, "workspace").unwrap();

        idx.delete_path(&remove).unwrap();

        // Python content from keep.md is still searchable.
        let keep_results = idx.search_fts("python", 10).unwrap();
        assert!(
            !keep_results.is_empty(),
            "chunks from other files must survive delete_path"
        );

        // Rust content from remove.md is gone.
        let remove_results = idx.search_fts("rust", 10).unwrap();
        assert!(
            remove_results.is_empty(),
            "chunks from deleted file must not appear"
        );
    }

    // -----------------------------------------------------------------------
    // access tracking + admin helper tests
    // -----------------------------------------------------------------------

    /// record_access increments access_count and sets last_accessed.
    #[test]
    fn test_record_access_increments_count() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let file = tmp.path().join("note.md");
        std::fs::write(&file, "# Guide\n\nRust testing.").unwrap();
        idx.reindex_file(&file, "workspace").unwrap();

        let path_str = file.to_string_lossy().to_string();
        let chunk_id = format!("{path_str}:0");

        // Initial access_count should be 0.
        let chunk_before = idx.get_chunk(&chunk_id).unwrap().unwrap();
        assert_eq!(chunk_before.access_count, 0, "access_count starts at 0");

        // Record two accesses.
        idx.record_access(&chunk_id).unwrap();
        idx.record_access(&chunk_id).unwrap();

        let chunk_after = idx.get_chunk(&chunk_id).unwrap().unwrap();
        assert_eq!(chunk_after.access_count, 2, "access_count must increment");
    }

    /// get_reindex_claim returns empty string when no claim is active.
    #[test]
    fn test_get_reindex_claim_empty_by_default() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);
        assert_eq!(
            idx.get_reindex_claim(),
            "",
            "claim must be empty on fresh index"
        );
    }

    /// all_indexed_paths returns distinct paths for indexed chunks.
    #[test]
    fn test_all_indexed_paths() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        let f1 = tmp.path().join("a.md");
        let f2 = tmp.path().join("b.md");
        std::fs::write(&f1, "# Alpha\n\nalpha content.").unwrap();
        std::fs::write(&f2, "# Beta\n\nbeta content.").unwrap();
        idx.reindex_file(&f1, "workspace").unwrap();
        idx.reindex_file(&f2, "workspace").unwrap();

        let paths = idx.all_indexed_paths().unwrap();
        let path_strs: Vec<&str> = paths.iter().map(String::as_str).collect();

        assert!(
            path_strs.contains(&f1.to_string_lossy().as_ref()),
            "a.md must be in indexed paths"
        );
        assert!(
            path_strs.contains(&f2.to_string_lossy().as_ref()),
            "b.md must be in indexed paths"
        );
        assert_eq!(
            paths.len(),
            2,
            "exactly two distinct paths should be indexed"
        );
    }

    /// all_indexed_paths returns empty for a fresh empty index.
    #[test]
    fn test_all_indexed_paths_empty_index() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);
        let paths = idx.all_indexed_paths().unwrap();
        assert!(paths.is_empty(), "fresh index has no indexed paths");
    }

    // -----------------------------------------------------------------------
    // reindex maintenance path regression tests
    // -----------------------------------------------------------------------

    /// Index a file; Delete the file from disk (simulates a user removing a session log); Run the same orphan-removal logic as `grok memory reindex`: compare `all_indexed_paths()` against current files and call `delete_path()` for paths that no longer exist; Verify the stale chunks are gone and are no longer searchable.
    /// This proves that `grok memory reindex`'s Phase 1 fixes the state that `grok memory doctor` warns about.
    #[test]
    fn test_reindex_maintenance_removes_orphaned_chunks() {
        let tmp = TempDir::new().unwrap();
        let mut idx = test_index(&tmp);

        // Seed the index with a file containing uniquely-identifiable content.
        let file = tmp.path().join("session.md");
        std::fs::write(&file, "# Session\n\nXyzzy-orphan-regression-token.").unwrap();
        idx.reindex_file(&file, "workspace").unwrap();

        // Precondition: the content is searchable.
        let before = idx.search_fts("orphan-regression-token", 10).unwrap();
        assert!(
            !before.is_empty(),
            "should find content before file is deleted"
        );

        // Delete the file; now it is orphaned in the index
        std::fs::remove_file(&file).unwrap();

        // Simulate `grok memory reindex` Phase 1: compare indexed vs current.
        let current: std::collections::BTreeSet<String> = vec![].into_iter().collect(); // Empty means no files exist.
        let indexed = idx.all_indexed_paths().unwrap();
        for path in &indexed {
            if !current.contains(path) {
                idx.delete_path(std::path::Path::new(path)).unwrap();
            }
        }

        // Postcondition: stale chunks are gone; search returns nothing.
        let after = idx.search_fts("orphan-regression-token", 10).unwrap();
        assert!(
            after.is_empty(),
            "orphaned chunks must be removed after reindex maintenance"
        );
    }

    /// A fresh (non-stale) reindex claim blocks `try_claim_reindex`.
    /// Verifies that `grok memory reindex` Phase 0 bails when a live session holds a fresh claim.
    /// The CLI cannot steal a live session's lock and then mutate the index concurrently.
    #[test]
    fn test_try_claim_reindex_fails_when_fresh_claim_held() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        // First caller claims unconditionally: a stale threshold of i64::MAX means any pre-existing claim is already "expired"
        let first = idx.try_claim_reindex(i64::MAX);
        assert!(first, "first claim should succeed on fresh index");

        // A second caller with STALE_SECS=60 must fail because the claim was set moments ago and is not yet older than 60 seconds
        let second = idx.try_claim_reindex(60);
        assert!(
            !second,
            "CLI reindex must not steal a fresh claim held by a live session"
        );

        // After the first owner releases, the CLI can claim successfully.
        idx.release_claim();
        let third = idx.try_claim_reindex(60);
        assert!(
            third,
            "CLI reindex must succeed after the live session releases"
        );
        idx.release_claim();
    }

    /// `grok memory reindex` Phase 3 resets the stale reindex claim.
    ///
    /// Verifies that `release_claim()` clears `meta.reindex_claim` so that `grok memory doctor` no longer reports a stale lock after reindex runs.
    #[test]
    fn test_reindex_maintenance_resets_stale_claim() {
        let tmp = TempDir::new().unwrap();
        let idx = test_index(&tmp);

        // Acquire the claim using stale_secs=0 (steals any existing claim) so we start with a known non-empty claim value
        let claimed = idx.try_claim_reindex(0);
        assert!(claimed, "try_claim_reindex should succeed on a fresh index");
        assert!(
            !idx.get_reindex_claim().is_empty(),
            "claim must be set before Phase 3"
        );

        // Simulate `grok memory reindex` Phase 3: release the claim.
        idx.release_claim();

        assert_eq!(
            idx.get_reindex_claim(),
            "",
            "release_claim must reset the claim so doctor reports no stale lock"
        );
    }
    // ZMY SELF-ADDED CODE START
    #[test]
    fn fork_lexical_search_recalls_separate_chinese_keywords() {
        let tmp = TempDir::new().unwrap();
        let mut index = MemoryIndex::initialize_fts_only(
            rusqlite::Connection::open_in_memory().unwrap(),
            test_storage(&tmp),
            MemoryIndexConfig::default(),
            4,
        )
        .unwrap();
        let separated = tmp.path().join("a-separated.md");
        let phrase = tmp.path().join("z-phrase.md");
        let global = tmp.path().join("global.md");
        index
            .reindex_content(
                &separated,
                "workspace",
                "部署要求兼容旧系统，服务端口使用8080。",
            )
            .unwrap();
        index
            .reindex_content(&phrase, "workspace", "部署 端口")
            .unwrap();
        index
            .reindex_content(&global, "global", "服务端口使用9090。")
            .unwrap();
        let separated_id = format!("{}:0", separated.display());
        let phrase_id = format!("{}:0", phrase.display());
        let global_id = format!("{}:0", global.display());

        for query in ["部署 端口", "部署，端口", "C++ 端口"] {
            let results = index.search_fts(query, 10).unwrap();
            assert!(
                results.iter().any(|r| r.chunk_id == separated_id),
                "{query}"
            );
            assert!(
                results.iter().any(|r| r.chunk_id == global_id),
                "OR semantics: {query}"
            );
            let scoped = index
                .search_fts_by_sources(query, 10, &["workspace"])
                .unwrap();
            assert!(scoped.iter().any(|r| r.chunk_id == separated_id));
            assert!(scoped.iter().all(|r| r.chunk_id != global_id));
        }
        assert_eq!(
            index.search_fts("部署 端口", 1).unwrap()[0].chunk_id,
            phrase_id
        );
        assert_eq!(
            index
                .search_fts_by_sources("部署 端口", 1, &["workspace"])
                .unwrap()[0]
                .chunk_id,
            phrase_id
        );
        let limited = index.search_fts("部署 端口 部署", 2).unwrap();
        assert_eq!(limited.len(), 2);
        assert_ne!(limited[0].chunk_id, limited[1].chunk_id);
        let long_query = format!("{} 部署 端口", "context ".repeat(150));
        assert!(
            index
                .search_fts(&long_query, 10)
                .unwrap()
                .iter()
                .any(|r| r.chunk_id == separated_id)
        );
        assert!(
            index
                .search_fts_by_sources("部署 端口", 10, &[])
                .unwrap()
                .is_empty()
        );
        assert!(index.search_fts("部署 端口", 0).unwrap().is_empty());
    }

    #[test]
    fn fork_vector_invalidation_survives_failed_initialization_and_recovery() {
        init_sqlite_vec();
        for action in ["edit", "truncate", "delete", "recreate"] {
            for reopen in [false, true] {
                let tmp = TempDir::new().unwrap();
                let db_path = tmp.path().join("test.sqlite");
                let open = |dimensions| {
                    MemoryIndex::open_or_create(
                        &db_path,
                        test_storage(&tmp),
                        MemoryIndexConfig::default(),
                        dimensions,
                    )
                    .unwrap()
                };
                let path = tmp.path().join("edited.md");
                let untouched = tmp.path().join("untouched.md");
                let id = format!("{}:0", path.display());
                let untouched_id = format!("{}:0", untouched.display());
                let mut healthy = open(4);
                healthy
                    .reindex_content(&path, "workspace", "oldcachetext")
                    .unwrap();
                healthy
                    .reindex_content(&untouched, "workspace", "stabletext")
                    .unwrap();
                healthy.upsert_embedding(&id, &[0.1; 4]).unwrap();
                healthy.upsert_embedding(&untouched_id, &[0.2; 4]).unwrap();

                let mut fallback = open(999999);
                assert!(!fallback.vec_available());
                match action {
                    "edit" => {
                        fallback
                            .reindex_content(&path, "workspace", "newcachetext")
                            .unwrap();
                    }
                    "truncate" => {
                        fallback.reindex_content(&path, "workspace", "").unwrap();
                    }
                    "delete" | "recreate" => {
                        fallback.delete_path(&path).unwrap();
                    }
                    _ => unreachable!(),
                }
                if action == "recreate" {
                    fallback
                        .reindex_content(&path, "workspace", "newcachetext")
                        .unwrap();
                }
                let has_text = action == "edit" || action == "recreate";
                assert_eq!(
                    fallback.search_fts("newcachetext", 10).unwrap().len(),
                    usize::from(has_text)
                );
                drop(fallback);

                if reopen {
                    drop(healthy);
                    healthy = open(4);
                }
                // An already-open vector connection must also observe the invalidation.
                let vectors = healthy.vector_search(&[0.1; 4], 10).unwrap();
                assert!(
                    vectors.iter().all(|(chunk_id, _)| chunk_id != &id),
                    "{action}, reopen={reopen}"
                );
                assert!(
                    vectors
                        .iter()
                        .any(|(chunk_id, _)| chunk_id == &untouched_id)
                );
                let pending = healthy.chunks_without_embeddings().unwrap();
                assert_eq!(pending.len(), usize::from(has_text));
                if has_text {
                    assert_eq!(pending[0], (id.clone(), "newcachetext".to_owned()));
                    healthy.upsert_embedding(&id, &[0.8; 4]).unwrap();
                    assert!(healthy.chunks_without_embeddings().unwrap().is_empty());
                    assert_eq!(healthy.vector_search(&[0.8; 4], 1).unwrap()[0], (id, 0.0));
                }
            }
        }
    }

    #[test]
    fn fork_failed_vector_migration_keeps_fts_and_old_cache() {
        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("test.sqlite");
        let storage = test_storage(&tmp);
        let file = tmp.path().join("note.md");
        std::fs::write(&file, "# Durable\n\nlexical migration rollback token").unwrap();
        let mut index =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 4)
                .unwrap();
        index.reindex_file(&file, "workspace").unwrap();
        let (chunk_id, _) = index
            .chunks_without_embeddings()
            .unwrap()
            .into_iter()
            .next()
            .expect("sqlite-vec should expose the new chunk");
        index
            .upsert_embedding(&chunk_id, &[0.1, 0.2, 0.3, 0.4])
            .unwrap();
        index
            .db()
            .execute_batch(
                "CREATE TRIGGER fail_embedding_dimensions BEFORE INSERT ON meta
                 WHEN NEW.key = 'embedding_dimensions'
                 BEGIN SELECT RAISE(ABORT, 'test migration failure'); END;",
            )
            .unwrap();
        let failed =
            MemoryIndex::open_or_create(&db_path, storage.clone(), MemoryIndexConfig::default(), 8)
                .unwrap();
        assert!(!failed.vec_available());
        assert!(!failed.search_fts("rollback token", 10).unwrap().is_empty());
        let stored: String = failed
            .db()
            .query_row(
                schema::GET_META_SQL,
                params!["embedding_dimensions"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, "4");
        let cached: i64 = failed
            .db()
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |row| row.get(0))
            .unwrap();
        assert_eq!(cached, 1);
        drop(failed);
        let reopened = MemoryIndex::open_or_create_preserving_dimensions(
            &db_path,
            storage,
            MemoryIndexConfig::default(),
            1024,
        )
        .unwrap();
        assert_eq!(reopened.embedding_dimensions(), 4);
        assert!(
            !reopened
                .search_fts("rollback token", 10)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn fork_invalid_dimensions_and_metadata_keep_fts_operational() {
        init_sqlite_vec();
        {
            let tmp = TempDir::new().unwrap();
            let db_path = tmp.path().join("zero-dimension.sqlite");
            let storage = test_storage(&tmp);
            let file = tmp.path().join("note.md");
            std::fs::write(&file, "# FTS fallback\n\nzero dimension lexical token").unwrap();
            let mut index =
                MemoryIndex::open_or_create(&db_path, storage, MemoryIndexConfig::default(), 0)
                    .unwrap();
            index.reindex_file(&file, "workspace").unwrap();
            assert!(!index.search_fts("lexical token", 10).unwrap().is_empty());
        }
        for (case, metadata) in [
            ("invalid", Some("not-a-dimension")),
            ("nonpositive", Some("0")),
            ("missing", None),
        ] {
            let tmp = TempDir::new().unwrap();
            let db_path = tmp.path().join(format!("{case}.sqlite"));
            let storage = test_storage(&tmp);
            let file = tmp.path().join("note.md");
            std::fs::write(&file, "# FTS fallback\n\ninvalid metadata lexical token").unwrap();
            let mut index = MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                MemoryIndexConfig::default(),
                4,
            )
            .unwrap();
            assert!(index.vec_available());
            index.reindex_file(&file, "workspace").unwrap();
            let (chunk_id, _) = index
                .chunks_without_embeddings()
                .unwrap()
                .into_iter()
                .next()
                .unwrap();
            index
                .upsert_embedding(&chunk_id, &[0.1, 0.2, 0.3, 0.4])
                .unwrap();
            drop(index);

            let connection = rusqlite::Connection::open(&db_path).unwrap();
            if let Some(metadata) = metadata {
                connection
                    .execute(
                        schema::UPSERT_META_SQL,
                        params!["embedding_dimensions", metadata],
                    )
                    .unwrap();
            } else {
                connection
                    .execute("DELETE FROM meta WHERE key = 'embedding_dimensions'", [])
                    .unwrap();
            }
            drop(connection);

            let mut reopened = MemoryIndex::open_or_create_preserving_dimensions(
                &db_path,
                storage,
                MemoryIndexConfig::default(),
                8,
            )
            .unwrap();
            assert_eq!(reopened.embedding_dimensions(), 8);
            assert!(!reopened.vec_available());
            assert!(!reopened.search_fts("lexical token", 10).unwrap().is_empty());
            let vector_schema: String = reopened
                .db()
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'chunks_vec'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(vector_schema.contains("FLOAT[4]"));
            let cached: i64 = reopened
                .db()
                .query_row("SELECT COUNT(*) FROM chunks_vec", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cached, 1);
            let stored_metadata: Option<String> = reopened
                .db()
                .query_row(
                    schema::GET_META_SQL,
                    params!["embedding_dimensions"],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(stored_metadata.as_deref(), metadata);

            let new_file = tmp.path().join("new-note.md");
            std::fs::write(&new_file, "# New\n\npreserved lexical write token").unwrap();
            reopened.reindex_file(&new_file, "workspace").unwrap();
            assert!(
                !reopened
                    .search_fts("lexical write token", 10)
                    .unwrap()
                    .is_empty()
            );
            let cached_after_write: i64 = reopened
                .db()
                .query_row("SELECT COUNT(*) FROM chunks_vec", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cached_after_write, 1);
            let metadata_after_write: Option<String> = reopened
                .db()
                .query_row(
                    schema::GET_META_SQL,
                    params!["embedding_dimensions"],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(metadata_after_write.as_deref(), metadata);
        }
    }

    #[test]
    fn fork_missing_dimension_metadata_rebuilds_vectors_after_fts_edits() {
        init_sqlite_vec();
        for requested_dimensions in [4, 8] {
            let tmp = TempDir::new().unwrap();
            let db_path = tmp.path().join("missing-dimensions.sqlite");
            let storage = test_storage(&tmp);
            let edited_file = tmp.path().join("edited.md");
            let deleted_file = tmp.path().join("deleted.md");
            std::fs::write(&edited_file, "# Edited\n\noldcachedtoken").unwrap();
            std::fs::write(&deleted_file, "# Deleted\n\ndeletedcachedtoken").unwrap();

            let mut seeded = MemoryIndex::open_or_create(
                &db_path,
                storage.clone(),
                MemoryIndexConfig::default(),
                4,
            )
            .unwrap();
            assert!(seeded.vec_available());
            seeded.reindex_file(&edited_file, "workspace").unwrap();
            seeded.reindex_file(&deleted_file, "workspace").unwrap();
            let initial_chunks = seeded.chunks_without_embeddings().unwrap();
            assert_eq!(initial_chunks.len(), 2);
            let edited_id = format!("{}:0", edited_file.to_string_lossy());
            let deleted_id = format!("{}:0", deleted_file.to_string_lossy());
            for (chunk_id, _) in initial_chunks {
                seeded
                    .upsert_embedding(&chunk_id, &[0.25, 0.25, 0.25, 0.25])
                    .unwrap();
            }
            drop(seeded);

            let connection = rusqlite::Connection::open(&db_path).unwrap();
            connection
                .execute("DELETE FROM meta WHERE key = 'embedding_dimensions'", [])
                .unwrap();
            drop(connection);

            let mut offline = MemoryIndex::open_or_create_preserving_dimensions(
                &db_path,
                storage.clone(),
                MemoryIndexConfig::default(),
                1024,
            )
            .unwrap();
            assert!(!offline.vec_available());
            std::fs::write(&edited_file, "# Edited\n\nreplacementcachedtoken").unwrap();
            let edit_result = offline.reindex_file(&edited_file, "workspace").unwrap();
            assert_eq!(edit_result.updated, 1);
            assert_eq!(edit_result.added, 0);
            assert!(
                !offline
                    .search_fts("replacementcachedtoken", 10)
                    .unwrap()
                    .is_empty()
            );
            assert!(offline.search_fts("oldcachedtoken", 10).unwrap().is_empty());
            assert_eq!(offline.delete_path(&deleted_file).unwrap(), 1);
            assert!(
                offline
                    .search_fts("deletedcachedtoken", 10)
                    .unwrap()
                    .is_empty()
            );

            let mut statement = offline
                .db()
                .prepare("SELECT chunk_id FROM chunks_vec ORDER BY chunk_id")
                .unwrap();
            let cached_ids = statement
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            let mut expected_cached_ids = vec![edited_id.clone(), deleted_id.clone()];
            expected_cached_ids.sort_unstable();
            assert_eq!(cached_ids, expected_cached_ids);
            let stored_dimensions: Option<String> = offline
                .db()
                .query_row(
                    schema::GET_META_SQL,
                    params!["embedding_dimensions"],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            assert_eq!(stored_dimensions, None);
            drop(statement);
            drop(offline);

            let recovered = MemoryIndex::open_or_create(
                &db_path,
                storage,
                MemoryIndexConfig::default(),
                requested_dimensions,
            )
            .unwrap();
            assert!(recovered.vec_available());
            assert_eq!(recovered.embedding_dimensions(), requested_dimensions);
            let missing = recovered.chunks_without_embeddings().unwrap();
            assert_eq!(missing.len(), 1);
            assert_eq!(missing[0].0, edited_id);
            assert!(missing[0].1.contains("replacementcachedtoken"));
            assert!(
                !recovered
                    .search_fts("replacementcachedtoken", 10)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                recovered
                    .search_fts("oldcachedtoken", 10)
                    .unwrap()
                    .is_empty()
            );
            assert!(
                recovered
                    .search_fts("deletedcachedtoken", 10)
                    .unwrap()
                    .is_empty()
            );
            let vector_schema: String = recovered
                .db()
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'chunks_vec'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(vector_schema.contains(&format!("FLOAT[{requested_dimensions}]")));
            let cached_after_recovery: i64 = recovered
                .db()
                .query_row("SELECT COUNT(*) FROM chunks_vec", [], |row| row.get(0))
                .unwrap();
            assert_eq!(cached_after_recovery, 0);
            let stored_dimensions: String = recovered
                .db()
                .query_row(
                    schema::GET_META_SQL,
                    params!["embedding_dimensions"],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(stored_dimensions, requested_dimensions.to_string());

            recovered
                .upsert_embedding(&edited_id, &vec![0.5; requested_dimensions])
                .unwrap();
            assert!(recovered.chunks_without_embeddings().unwrap().is_empty());
        }
    }
    // ZMY SELF-ADDED CODE END

    // ZMY SELF-ADDED CODE START
    #[test]
    fn fork_lexical_search_recalls_literal_chinese_technical_and_operator_queries() {
        init_sqlite_vec();
        let tmp = TempDir::new().unwrap();
        let mut index = test_index(&tmp);
        let workspace = tmp.path().join("workspace.md");
        let global = tmp.path().join("global.md");
        let technical = tmp.path().join("technical.md");
        let plain_c = tmp.path().join("plain-c.md");
        let literal = tmp.path().join("literal.md");
        let graphite = tmp.path().join("graphite.md");
        std::fs::write(&workspace, "# 部署\n\n部署").unwrap();
        std::fs::write(&global, "# Chinese requirement\n\n部署要求兼容旧系统。").unwrap();
        std::fs::write(
            &technical,
            "# Listener\n\nThe listener uses port 8080 and a C++ service.",
        )
        .unwrap();
        std::fs::write(&plain_c, "# C service\n\nA plain C service.").unwrap();
        std::fs::write(
            &literal,
            "# Literal query\n\nThe exact input is \"the OR a++\"; payload %' OR 1=1 --.",
        )
        .unwrap();
        std::fs::write(
            &graphite,
            "# Graphite deployment\n\nGraphite deployment notes for workspace memory.",
        )
        .unwrap();
        index.reindex_file(&workspace, "workspace").unwrap();
        index.reindex_file(&global, "global").unwrap();
        index.reindex_file(&technical, "workspace").unwrap();
        index.reindex_file(&plain_c, "workspace").unwrap();
        index.reindex_file(&literal, "workspace").unwrap();
        index.reindex_file(&graphite, "workspace").unwrap();

        let chinese = index.search_fts("部署", 10).unwrap();
        assert_eq!(chinese.len(), 2);
        assert!(chinese.iter().any(|result| {
            result
                .chunk_id
                .starts_with(workspace.to_string_lossy().as_ref())
        }));
        let workspace_results = index
            .search_fts_by_sources("部署", 10, &["workspace"])
            .unwrap();
        assert_eq!(workspace_results.len(), 1);
        assert_eq!(
            workspace_results[0].chunk_id,
            format!("{}:0", workspace.to_string_lossy())
        );
        let global_results = index
            .search_fts_by_sources("部署", 10, &["global"])
            .unwrap();
        assert_eq!(global_results.len(), 1);
        assert!(global_results.iter().all(|result| {
            result
                .chunk_id
                .starts_with(global.to_string_lossy().as_ref())
        }));
        let chinese_single = index.search_fts("署", 10).unwrap();
        assert!(
            chinese_single
                .iter()
                .any(|result| result.chunk_id == format!("{}:0", global.to_string_lossy()))
        );

        let port = index.search_fts("8080", 10).unwrap();
        assert!(
            port.iter()
                .any(|result| result.chunk_id == format!("{}:0", technical.to_string_lossy()))
        );
        let cpp = index.search_fts("C++", 10).unwrap();
        assert!(
            cpp.iter()
                .any(|result| result.chunk_id == format!("{}:0", technical.to_string_lossy()))
        );
        assert_eq!(
            index.search_fts("C++", 1).unwrap()[0].chunk_id,
            format!("{}:0", technical.to_string_lossy())
        );

        let operator = index.search_fts("\"the OR a++\"", 10).unwrap();
        assert_eq!(operator.len(), 1);
        assert_eq!(
            operator[0].chunk_id,
            format!("{}:0", literal.to_string_lossy())
        );
        let english = index.search_fts("listener port service", 10).unwrap();
        assert!(
            english
                .iter()
                .any(|result| result.chunk_id == format!("{}:0", technical.to_string_lossy()))
        );
        let graphite_id = format!("{}:0", graphite.to_string_lossy());
        let long_prompt = format!(
            "graphite deployment {}",
            "conversational context ".repeat(60)
        );
        assert!(long_prompt.chars().count() > 1200);
        assert!(
            index
                .search_fts(&long_prompt, 10)
                .unwrap()
                .iter()
                .any(|result| result.chunk_id == graphite_id)
        );
        assert!(
            index
                .search_fts_by_sources(&long_prompt, 10, &["workspace"])
                .unwrap()
                .iter()
                .any(|result| result.chunk_id == graphite_id)
        );
        let overlong_token_prompt = format!("{} graphite deployment", "x".repeat(1025));
        assert!(
            index
                .search_fts(&overlong_token_prompt, 10)
                .unwrap()
                .iter()
                .any(|result| result.chunk_id == graphite_id)
        );
        assert!(
            index
                .search_fts_by_sources(&overlong_token_prompt, 10, &["workspace"])
                .unwrap()
                .iter()
                .any(|result| result.chunk_id == graphite_id)
        );
        assert!(index.search_fts("   ", 10).unwrap().is_empty());
        assert!(index.search_fts("++---", 10).unwrap().is_empty());
        assert!(index.search_fts("部署要求兼容旧", 0).unwrap().is_empty());
        assert!(
            index
                .search_fts("界".repeat(1025).as_str(), 10)
                .unwrap()
                .is_empty()
        );
        assert!(
            index
                .search_fts_by_sources("部署要求兼容旧", 10, &[])
                .unwrap()
                .is_empty()
        );
        assert!(chinese.iter().all(|result| result.rank < 0.0));
    }
    // ZMY SELF-ADDED CODE END
}
