use std::{fs, io, num::NonZeroUsize, path::Path, sync::Mutex, time::SystemTime};

use csv::ReaderBuilder;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use crate::error::{Error, Result};

#[derive(Debug, Deserialize)]
pub struct Candidate {
    pub text: String,
    pub code: String,
    pub weight: Option<f64>,
    pub meta: Option<String>,
}

/// The state of one exact-code candidate query, including its current page.
/// Candidate rows are loaded only for the current page.
#[derive(Debug)]
pub struct Query {
    code: String,
    total_count: usize,
    page_size: NonZeroUsize,
    page_index: usize,
    candidates: Vec<String>,
}

impl Query {
    #[allow(unused)]
    pub fn code(&self) -> &str {
        &self.code
    }

    pub fn candidates(&self) -> &[String] {
        &self.candidates
    }

    pub fn total_count(&self) -> usize {
        self.total_count
    }

    #[allow(unused)]
    pub fn page_size(&self) -> NonZeroUsize {
        self.page_size
    }

    pub fn page_index(&self) -> usize {
        self.page_index
    }

    pub fn page_count(&self) -> usize {
        if self.total_count == 0 {
            0
        } else {
            1 + (self.total_count - 1) / self.page_size.get()
        }
    }

    pub fn has_previous_page(&self) -> bool {
        self.page_index > 0
    }

    pub fn has_next_page(&self) -> bool {
        self.page_index + 1 < self.page_count()
    }
}

/// SQLite-backed dictionary engine.
pub struct Engine {
    db: Mutex<Connection>,
}

impl Engine {
    /// Load the TSV dictionary into a persistent SQLite cache.
    ///
    /// The caller owns the cache location; the engine deliberately has no default
    /// filesystem path so applications can choose their platform-specific layout.
    pub fn load(dict_path: impl AsRef<Path>, cache_path: impl AsRef<Path>) -> Result<Self> {
        let dict_path = dict_path.as_ref();
        let cache_path = cache_path.as_ref();
        let dict_modified = fs::metadata(dict_path)?.modified()?;

        if let Some(parent) = cache_path.parent().filter(|p| !p.as_os_str().is_empty()) {
            fs::create_dir_all(parent)?;
        }

        let cache_is_stale = match fs::metadata(cache_path) {
            Ok(metadata) => {
                let cache_modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                cache_modified < dict_modified || !cache_schema_is_valid(cache_path)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(error.into()),
        };

        if cache_is_stale {
            rebuild_cache(dict_path, cache_path)?;
        }

        let conn = Connection::open(cache_path)?;
        Ok(Self {
            db: Mutex::new(conn),
        })
    }

    /// Start an exact-code query and load its first page.
    /// The total candidate count is read once from `code_stats`; subsequent page
    /// loads reuse it and execute only the paginated candidate query.
    pub fn query(&self, code: &str, page_size: NonZeroUsize) -> Result<Query> {
        let db = self.db.lock().expect("SQLite connection mutex poisoned");
        let total_count = db
            .query_row(
                "SELECT candidate_count FROM code_stats WHERE code = ?1",
                [code],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX))
            .unwrap_or(0);

        let mut query = Query {
            code: code.to_owned(),
            total_count,
            page_size,
            page_index: 0,
            candidates: Vec::new(),
        };
        if query.total_count > 0 {
            query.candidates = Self::query_page_locked(&db, &query.code, 0, query.page_size)?;
        }
        Ok(query)
    }

    /// Load a page for an existing query without recounting the candidates.
    pub fn load_page(&self, query: &mut Query, page_index: usize) -> Result<()> {
        if page_index >= query.page_count() {
            return Err(Error::PageOutOfRange);
        }

        let db = self.db.lock().expect("SQLite connection mutex poisoned");
        let candidates = Self::query_page_locked(&db, &query.code, page_index, query.page_size)?;
        query.candidates = candidates;
        query.page_index = page_index;
        Ok(())
    }

    fn query_page_locked(
        db: &Connection,
        code: &str,
        page_index: usize,
        page_size: NonZeroUsize,
    ) -> Result<Vec<String>> {
        let offset = page_index.saturating_mul(page_size.get());
        let limit = i64::try_from(page_size.get()).unwrap_or(i64::MAX);
        let offset = i64::try_from(offset).unwrap_or(i64::MAX);

        let mut stmt = db.prepare_cached(
            r#"
            SELECT text
            FROM candidates
            WHERE code = ?1
            ORDER BY weight DESC, seq ASC
            LIMIT ?2 OFFSET ?3
            "#,
        )?;
        let rows = stmt.query_map(params![code, limit, offset], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}

fn cache_schema_is_valid(path: &Path) -> bool {
    let Ok(conn) = Connection::open(path) else {
        return false;
    };
    let version_is_valid = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .is_ok_and(|version| version == 2);
    let schema_present = conn
        .query_row(
            r#"
            SELECT
                EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'candidates'),
                EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'candidates_by_code_weight'),
                EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'code_stats')
            "#,
            [],
            |row| {
                Ok((
                    row.get::<_, bool>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .is_ok_and(|(table, index, stats)| table && index && stats);

    version_is_valid
        && schema_present
        && conn
            .prepare("SELECT seq, code, text, weight, meta FROM candidates LIMIT 0")
            .is_ok()
        && conn
            .prepare("SELECT code, candidate_count FROM code_stats LIMIT 0")
            .is_ok()
}

fn rebuild_cache(dict_path: &Path, cache_path: &Path) -> Result<()> {
    let dict_file = fs::File::open(dict_path)?;
    let cache_name = cache_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("dict.sqlite");
    let temp_path = cache_path.with_file_name(format!(".{cache_name}.tmp-{}", std::process::id()));

    if temp_path.exists() {
        fs::remove_file(&temp_path)?;
    }

    let build_result = (|| -> Result<()> {
        let mut conn = Connection::open(&temp_path)?;
        // This is a rebuildable cache; avoid paying for durable intermediate writes.
        conn.execute_batch(
            r#"
            PRAGMA journal_mode = DELETE;
            PRAGMA synchronous = OFF;
            CREATE TABLE candidates (
                seq    INTEGER PRIMARY KEY,
                code   TEXT NOT NULL COLLATE BINARY,
                text   TEXT NOT NULL,
                weight REAL,
                meta   TEXT
            );
            "#,
        )?;

        {
            let tx = conn.transaction()?;
            {
                let mut insert = tx.prepare(
                    r#"
                    INSERT INTO candidates (code, text, weight, meta)
                    VALUES (?1, ?2, ?3, ?4)
                    "#,
                )?;
                let mut reader = ReaderBuilder::new()
                    .flexible(true)
                    .delimiter(b'\t')
                    .from_reader(dict_file);

                for row in reader.deserialize::<Candidate>() {
                    let mut candidate = row?;
                    // Whitespace in a code does not affect matching.
                    candidate.code = candidate.code.replace(' ', "");
                    insert.execute(params![
                        candidate.code,
                        candidate.text,
                        candidate.weight,
                        candidate.meta,
                    ])?;
                }
            }
            tx.commit()?;
        }

        // Build indexes and exact-code counts after the bulk import.
        conn.execute_batch(
            r#"
            CREATE INDEX candidates_by_code_weight
            ON candidates (code COLLATE BINARY, weight DESC, seq ASC);

            CREATE TABLE code_stats (
                code            TEXT PRIMARY KEY COLLATE BINARY,
                candidate_count INTEGER NOT NULL CHECK (candidate_count > 0)
            ) WITHOUT ROWID;

            INSERT INTO code_stats (code, candidate_count)
            SELECT code, COUNT(*)
            FROM candidates
            GROUP BY code;

            PRAGMA user_version = 2;
            "#,
        )?;
        drop(conn);
        fs::rename(&temp_path, cache_path)?;
        Ok(())
    })();

    if build_result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    build_result
}
