use std::{fs, path::PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, params};

use crate::{config, git::GitRepo, materializer::MaterializationKind, reporting::Status};

pub struct State {
    conn: Option<Connection>,
}

pub struct ShimRecord<'a> {
    pub repo: &'a GitRepo,
    pub adapter_name: &'a str,
    pub source_rel_path: &'a str,
    pub target_rel_path: &'a str,
    pub materialization: Option<MaterializationKind>,
    pub content_hash: Option<String>,
    pub status: Status,
    pub message: &'a str,
}

#[derive(Debug, Clone)]
pub struct StoredShim {
    pub materialization: Option<String>,
    pub content_hash: Option<String>,
}

impl State {
    pub fn open_default() -> Result<Self> {
        Self::open(config::data_dir()?)
    }

    pub fn open_default_read_only_if_exists() -> Result<Self> {
        Self::open_read_only_if_exists(config::data_dir()?)
    }

    pub fn open(data_dir: PathBuf) -> Result<Self> {
        fs::create_dir_all(&data_dir)
            .with_context(|| format!("failed to create data dir {}", data_dir.display()))?;
        let db_path = data_dir.join("state.sqlite3");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open state db {}", db_path.display()))?;
        let state = Self { conn: Some(conn) };
        state.migrate()?;
        Ok(state)
    }

    pub fn open_read_only_if_exists(data_dir: PathBuf) -> Result<Self> {
        let db_path = data_dir.join("state.sqlite3");
        if !db_path.exists() {
            return Ok(Self::disabled());
        }

        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open state db {}", db_path.display()))?;
        Ok(Self { conn: Some(conn) })
    }

    pub fn disabled() -> Self {
        Self { conn: None }
    }

    pub fn record(&self, record: ShimRecord<'_>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        conn.execute(
            r#"
            INSERT INTO repositories
              (root_path, git_dir, exclude_path, first_seen_at, last_seen_at, last_reconciled_at, last_error)
            VALUES (?1, ?2, ?3, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL)
            ON CONFLICT(root_path) DO UPDATE SET
              git_dir = excluded.git_dir,
              exclude_path = excluded.exclude_path,
              last_seen_at = CURRENT_TIMESTAMP,
              last_reconciled_at = CURRENT_TIMESTAMP,
              last_error = NULL
            "#,
            params![
                record.repo.root.to_string_lossy(),
                record.repo.git_dir.to_string_lossy(),
                record.repo.exclude_path.to_string_lossy(),
            ],
        )?;

        let repo_id: i64 = conn.query_row(
            "SELECT id FROM repositories WHERE root_path = ?1",
            params![record.repo.root.to_string_lossy()],
            |row| row.get(0),
        )?;

        let materialization = record
            .materialization
            .map(|kind| format!("{kind:?}").to_lowercase());
        let status = format!("{:?}", record.status).to_lowercase();

        conn.execute(
            r#"
            INSERT INTO shims
              (repository_id, adapter_name, source_rel_path, target_rel_path, materialization,
               content_hash, created_at, last_seen_at, last_reconciled_at, last_status, last_error)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP,
                    CURRENT_TIMESTAMP, ?7, NULL)
            ON CONFLICT(repository_id, adapter_name, target_rel_path) DO UPDATE SET
              source_rel_path = excluded.source_rel_path,
              materialization = excluded.materialization,
              content_hash = excluded.content_hash,
              last_seen_at = CURRENT_TIMESTAMP,
              last_reconciled_at = CURRENT_TIMESTAMP,
              last_status = excluded.last_status,
              last_error = NULL
            "#,
            params![
                repo_id,
                record.adapter_name,
                record.source_rel_path,
                record.target_rel_path,
                materialization,
                record.content_hash,
                status,
            ],
        )?;

        conn.execute(
            r#"
            INSERT INTO events
              (occurred_at, level, repository_path, adapter_name, action, message)
            VALUES (CURRENT_TIMESTAMP, ?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                if record.status == Status::Error {
                    "error"
                } else {
                    "info"
                },
                record.repo.root.to_string_lossy(),
                record.adapter_name,
                status,
                record.message,
            ],
        )?;

        Ok(())
    }

    pub fn get_shim(
        &self,
        repo: &GitRepo,
        adapter_name: &str,
        target_rel_path: &str,
    ) -> Result<Option<StoredShim>> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };

        let mut statement = conn.prepare(
            r#"
            SELECT s.materialization, s.content_hash
            FROM shims s
            JOIN repositories r ON r.id = s.repository_id
            WHERE r.root_path = ?1
              AND s.adapter_name = ?2
              AND s.target_rel_path = ?3
            "#,
        )?;

        let mut rows = statement.query(params![
            repo.root.to_string_lossy(),
            adapter_name,
            target_rel_path,
        ])?;

        if let Some(row) = rows.next()? {
            Ok(Some(StoredShim {
                materialization: row.get(0)?,
                content_hash: row.get(1)?,
            }))
        } else {
            Ok(None)
        }
    }

    fn migrate(&self) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS repositories (
              id INTEGER PRIMARY KEY,
              root_path TEXT NOT NULL UNIQUE,
              git_dir TEXT NOT NULL,
              exclude_path TEXT NOT NULL,
              first_seen_at TEXT NOT NULL,
              last_seen_at TEXT NOT NULL,
              last_reconciled_at TEXT NOT NULL,
              last_error TEXT
            );

            CREATE TABLE IF NOT EXISTS shims (
              id INTEGER PRIMARY KEY,
              repository_id INTEGER NOT NULL,
              adapter_name TEXT NOT NULL,
              source_rel_path TEXT NOT NULL,
              target_rel_path TEXT NOT NULL,
              materialization TEXT,
              target_kind TEXT,
              content_hash TEXT,
              created_at TEXT NOT NULL,
              last_seen_at TEXT NOT NULL,
              last_reconciled_at TEXT NOT NULL,
              last_status TEXT NOT NULL,
              last_error TEXT,
              UNIQUE(repository_id, adapter_name, target_rel_path),
              FOREIGN KEY(repository_id) REFERENCES repositories(id)
            );

            CREATE TABLE IF NOT EXISTS events (
              id INTEGER PRIMARY KEY,
              occurred_at TEXT NOT NULL,
              level TEXT NOT NULL,
              repository_path TEXT,
              adapter_name TEXT,
              action TEXT NOT NULL,
              message TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }
}
