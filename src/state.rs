use std::{
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;

use crate::{config, git::GitRepo, materializer::MaterializationKind, reporting::Status};

const STATE_VERSION: i64 = 2;
const STATE_FILE: &str = "state.sqlite3";
const CUTOVER_ARCHIVE: &str = "state.pre-observed-cutover.sqlite3";

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

pub struct TargetedShimRecord<'a> {
    pub repo: &'a GitRepo,
    pub instruction_dir: &'a Path,
    pub adapter_name: &'a str,
    pub source_rel_path: &'a str,
    pub target_rel_path: &'a str,
    pub materialization: Option<MaterializationKind>,
    pub content_hash: Option<String>,
    pub status: Status,
    pub message: &'a str,
}

pub struct DetectedClaudeRecord<'a> {
    pub repo: &'a GitRepo,
    pub instruction_dir: &'a Path,
    pub claude_path: &'a Path,
    pub agents_path: &'a Path,
    pub classification: &'a str,
    pub content_hash: Option<&'a str>,
    pub status: Option<&'a str>,
    pub error: Option<&'a str>,
}

#[derive(Debug, Clone)]
pub struct StoredShim {
    pub materialization: Option<String>,
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ObservedInstructionDir {
    pub id: i64,
    pub repo_root: PathBuf,
    pub instruction_dir: PathBuf,
    pub source_rel_path: PathBuf,
    pub target_rel_path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ObservedRepoSummary {
    pub repo: PathBuf,
    pub instruction_dirs: usize,
    pub last_seen_at: Option<String>,
    pub last_reconciled_at: Option<String>,
    pub last_status: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DetectedClaudeFile {
    pub id: i64,
    pub repo_root: PathBuf,
    pub instruction_dir: PathBuf,
    pub claude_path: PathBuf,
    pub agents_path: PathBuf,
    pub classification: String,
    pub last_migration_status: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ManagedShim {
    pub id: i64,
    pub repo_root: PathBuf,
    pub instruction_dir: PathBuf,
    pub source_rel_path: PathBuf,
    pub target_rel_path: PathBuf,
    pub materialization: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct StateCounts {
    pub observed_repos: usize,
    pub instruction_dirs: usize,
    pub migration_candidates: usize,
    pub recent_errors: usize,
    pub last_reconciled_at: Option<String>,
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
        let db_path = data_dir.join(STATE_FILE);
        archive_old_state_if_needed(&db_path)?;
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open state db {}", db_path.display()))?;
        let state = Self { conn: Some(conn) };
        state.migrate()?;
        Ok(state)
    }

    pub fn open_read_only_if_exists(data_dir: PathBuf) -> Result<Self> {
        let db_path = data_dir.join(STATE_FILE);
        if !db_path.exists() {
            return Ok(Self::disabled());
        }

        let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("failed to open state db {}", db_path.display()))?;
        if user_version(&conn)? != STATE_VERSION {
            return Ok(Self::disabled());
        }
        Ok(Self { conn: Some(conn) })
    }

    pub fn disabled() -> Self {
        Self { conn: None }
    }

    pub fn record(&self, record: ShimRecord<'_>) -> Result<()> {
        self.record_targeted(TargetedShimRecord {
            repo: record.repo,
            instruction_dir: &record.repo.root,
            adapter_name: record.adapter_name,
            source_rel_path: record.source_rel_path,
            target_rel_path: record.target_rel_path,
            materialization: record.materialization,
            content_hash: record.content_hash,
            status: record.status,
            message: record.message,
        })
    }

    pub fn record_targeted(&self, record: TargetedShimRecord<'_>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        let instruction_dir_id = self.record_instruction_dir(
            record.repo,
            record.instruction_dir,
            record.source_rel_path,
            record.target_rel_path,
            "claude-hook",
            None,
        )?;
        let materialization = record
            .materialization
            .map(|kind| format!("{kind:?}").to_lowercase());
        let status = format!("{:?}", record.status).to_lowercase();

        conn.execute(
            r#"
            INSERT INTO shims
              (instruction_dir_id, adapter_name, source_rel_path, target_rel_path, materialization,
               content_hash, created_at, last_seen_at, last_reconciled_at, last_status, last_error)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP,
                    CURRENT_TIMESTAMP, ?7, NULL)
            ON CONFLICT(instruction_dir_id, adapter_name, target_rel_path) DO UPDATE SET
              source_rel_path = excluded.source_rel_path,
              materialization = excluded.materialization,
              content_hash = excluded.content_hash,
              last_seen_at = CURRENT_TIMESTAMP,
              last_reconciled_at = CURRENT_TIMESTAMP,
              last_status = excluded.last_status,
              last_error = NULL,
              removed_at = NULL
            "#,
            params![
                instruction_dir_id,
                record.adapter_name,
                record.source_rel_path,
                record.target_rel_path,
                materialization,
                record.content_hash,
                status,
            ],
        )?;

        self.mark_instruction_result(record.instruction_dir, Some(&status), None)?;
        self.record_event(
            if record.status == Status::Error {
                "error"
            } else {
                "info"
            },
            Some(&record.repo.root),
            Some(record.adapter_name),
            &status,
            record.message,
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

        conn.query_row(
            r#"
            SELECT s.materialization, s.content_hash
            FROM shims s
            JOIN observed_instruction_dirs d ON d.id = s.instruction_dir_id
            JOIN observed_repositories r ON r.id = d.repository_id
            WHERE r.root_path = ?1
              AND s.adapter_name = ?2
              AND s.target_rel_path = ?3
              AND s.removed_at IS NULL
            ORDER BY s.last_reconciled_at DESC
            LIMIT 1
            "#,
            params![repo.root.to_string_lossy(), adapter_name, target_rel_path,],
            |row| {
                Ok(StoredShim {
                    materialization: row.get(0)?,
                    content_hash: row.get(1)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn record_instruction_dir(
        &self,
        repo: &GitRepo,
        instruction_dir: &Path,
        source_rel_path: &str,
        target_rel_path: &str,
        source: &str,
        observed_cwd: Option<&Path>,
    ) -> Result<i64> {
        let Some(conn) = &self.conn else {
            return Ok(0);
        };

        let repo_id = self.ensure_repository(repo, source, observed_cwd)?;
        conn.execute(
            r#"
            INSERT INTO observed_instruction_dirs
              (repository_id, instruction_dir, source_rel_path, target_rel_path,
               first_seen_at, last_seen_at, last_reconciled_at, last_status, last_error, active)
            VALUES (?1, ?2, ?3, ?4, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, NULL, NULL, NULL, 1)
            ON CONFLICT(repository_id, instruction_dir) DO UPDATE SET
              source_rel_path = excluded.source_rel_path,
              target_rel_path = excluded.target_rel_path,
              last_seen_at = CURRENT_TIMESTAMP,
              active = 1
            "#,
            params![
                repo_id,
                instruction_dir.to_string_lossy(),
                source_rel_path,
                target_rel_path,
            ],
        )?;

        conn.query_row(
            "SELECT id FROM observed_instruction_dirs WHERE repository_id = ?1 AND instruction_dir = ?2",
            params![repo_id, instruction_dir.to_string_lossy()],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    pub fn record_detected_claude_file(&self, record: DetectedClaudeRecord<'_>) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        let repo_id = self.ensure_repository(record.repo, "claude-hook", None)?;
        conn.execute(
            r#"
            INSERT INTO detected_claude_files
              (repository_id, instruction_dir, claude_path, agents_path, classification,
               content_hash, first_seen_at, last_seen_at, last_migration_status, last_error, migrated_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP, ?7, ?8, NULL)
            ON CONFLICT(repository_id, claude_path) DO UPDATE SET
              instruction_dir = excluded.instruction_dir,
              agents_path = excluded.agents_path,
              classification = excluded.classification,
              content_hash = excluded.content_hash,
              last_seen_at = CURRENT_TIMESTAMP,
              last_migration_status = COALESCE(excluded.last_migration_status, last_migration_status),
              last_error = excluded.last_error
            "#,
            params![
                repo_id,
                record.instruction_dir.to_string_lossy(),
                record.claude_path.to_string_lossy(),
                record.agents_path.to_string_lossy(),
                record.classification,
                record.content_hash,
                record.status,
                record.error,
            ],
        )?;
        Ok(())
    }

    pub fn active_instruction_dirs(&self, limit: usize) -> Result<Vec<ObservedInstructionDir>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            r#"
            SELECT d.id, r.root_path, d.instruction_dir, d.source_rel_path, d.target_rel_path
            FROM observed_instruction_dirs d
            JOIN observed_repositories r ON r.id = d.repository_id
            WHERE r.active = 1 AND d.active = 1
            ORDER BY d.last_seen_at DESC, d.id DESC
            LIMIT ?1
            "#,
        )?;
        let rows = statement.query_map(params![limit as i64], |row| {
            Ok(ObservedInstructionDir {
                id: row.get(0)?,
                repo_root: PathBuf::from(row.get::<_, String>(1)?),
                instruction_dir: PathBuf::from(row.get::<_, String>(2)?),
                source_rel_path: PathBuf::from(row.get::<_, String>(3)?),
                target_rel_path: PathBuf::from(row.get::<_, String>(4)?),
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn observed_repos(&self) -> Result<Vec<ObservedRepoSummary>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            r#"
            SELECT r.root_path,
                   COUNT(d.id) FILTER (WHERE d.active = 1) AS instruction_dirs,
                   r.last_seen_at,
                   r.last_reconciled_at,
                   r.last_status,
                   r.last_error
            FROM observed_repositories r
            LEFT JOIN observed_instruction_dirs d ON d.repository_id = r.id
            WHERE r.active = 1
            GROUP BY r.id
            ORDER BY r.last_seen_at DESC, r.root_path
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ObservedRepoSummary {
                repo: PathBuf::from(row.get::<_, String>(0)?),
                instruction_dirs: row.get::<_, i64>(1)? as usize,
                last_seen_at: row.get(2)?,
                last_reconciled_at: row.get(3)?,
                last_status: row.get(4)?,
                last_error: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn migration_candidates(&self) -> Result<Vec<DetectedClaudeFile>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            r#"
            SELECT f.id, r.root_path, f.instruction_dir, f.claude_path, f.agents_path,
                   f.classification, f.last_migration_status, f.last_error
            FROM detected_claude_files f
            JOIN observed_repositories r ON r.id = f.repository_id
            WHERE r.active = 1
              AND f.migrated_at IS NULL
              AND f.classification IN ('user_file', 'tracked_user_file')
            ORDER BY f.last_seen_at DESC, f.claude_path
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok(DetectedClaudeFile {
                id: row.get(0)?,
                repo_root: PathBuf::from(row.get::<_, String>(1)?),
                instruction_dir: PathBuf::from(row.get::<_, String>(2)?),
                claude_path: PathBuf::from(row.get::<_, String>(3)?),
                agents_path: PathBuf::from(row.get::<_, String>(4)?),
                classification: row.get(5)?,
                last_migration_status: row.get(6)?,
                last_error: row.get(7)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn managed_shims(&self) -> Result<Vec<ManagedShim>> {
        let Some(conn) = &self.conn else {
            return Ok(Vec::new());
        };
        let mut statement = conn.prepare(
            r#"
            SELECT s.id, r.root_path, d.instruction_dir, s.source_rel_path, s.target_rel_path, s.materialization
            FROM shims s
            JOIN observed_instruction_dirs d ON d.id = s.instruction_dir_id
            JOIN observed_repositories r ON r.id = d.repository_id
            WHERE r.active = 1
              AND d.active = 1
              AND s.removed_at IS NULL
              AND s.materialization IS NOT NULL
            ORDER BY s.last_seen_at DESC, s.id DESC
            "#,
        )?;
        let rows = statement.query_map([], |row| {
            Ok(ManagedShim {
                id: row.get(0)?,
                repo_root: PathBuf::from(row.get::<_, String>(1)?),
                instruction_dir: PathBuf::from(row.get::<_, String>(2)?),
                source_rel_path: PathBuf::from(row.get::<_, String>(3)?),
                target_rel_path: PathBuf::from(row.get::<_, String>(4)?),
                materialization: row.get(5)?,
            })
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn mark_detected_migration_result(
        &self,
        id: i64,
        status: &str,
        error: Option<&str>,
        migrated: bool,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            r#"
            UPDATE detected_claude_files
            SET last_migration_status = ?2,
                last_error = ?3,
                migrated_at = CASE WHEN ?4 THEN CURRENT_TIMESTAMP ELSE migrated_at END
            WHERE id = ?1
            "#,
            params![id, status, error, migrated],
        )?;
        Ok(())
    }

    pub fn deactivate_repo(&self, repo: &Path) -> Result<bool> {
        let Some(conn) = &self.conn else {
            return Ok(false);
        };
        let changed = conn.execute(
            "UPDATE observed_repositories SET active = 0 WHERE root_path = ?1 AND active = 1",
            params![repo.to_string_lossy()],
        )?;
        Ok(changed > 0)
    }

    pub fn prune_missing(&self, forget_history: bool) -> Result<(usize, usize)> {
        let Some(conn) = &self.conn else {
            return Ok((0, 0));
        };
        let repos = self.observed_repos()?;
        let missing_repos = repos
            .into_iter()
            .filter(|repo| !repo.repo.exists())
            .map(|repo| repo.repo)
            .collect::<Vec<_>>();
        let mut repo_count = 0;
        for repo in missing_repos {
            if forget_history {
                repo_count += conn.execute(
                    "DELETE FROM observed_repositories WHERE root_path = ?1",
                    params![repo.to_string_lossy()],
                )?;
            } else {
                repo_count += conn.execute(
                    "UPDATE observed_repositories SET active = 0 WHERE root_path = ?1",
                    params![repo.to_string_lossy()],
                )?;
            }
        }

        let dirs = self.active_instruction_dirs(usize::MAX)?;
        let missing_dirs = dirs
            .into_iter()
            .filter(|dir| !dir.instruction_dir.exists())
            .map(|dir| dir.id)
            .collect::<Vec<_>>();
        let mut dir_count = 0;
        for id in missing_dirs {
            if forget_history {
                dir_count += conn.execute(
                    "DELETE FROM observed_instruction_dirs WHERE id = ?1",
                    params![id],
                )?;
            } else {
                dir_count += conn.execute(
                    "UPDATE observed_instruction_dirs SET active = 0 WHERE id = ?1",
                    params![id],
                )?;
            }
        }
        Ok((repo_count, dir_count))
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            r#"
            INSERT INTO settings (key, value, updated_at)
            VALUES (?1, ?2, CURRENT_TIMESTAMP)
            ON CONFLICT(key) DO UPDATE SET
              value = excluded.value,
              updated_at = CURRENT_TIMESTAMP
            "#,
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let Some(conn) = &self.conn else {
            return Ok(None);
        };
        conn.query_row(
            "SELECT value FROM settings WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn setting_bool(&self, key: &str, default: bool) -> Result<bool> {
        Ok(self
            .get_setting(key)?
            .map(|value| matches!(value.as_str(), "true" | "1" | "yes" | "on"))
            .unwrap_or(default))
    }

    pub fn counts(&self) -> Result<StateCounts> {
        let Some(conn) = &self.conn else {
            return Ok(StateCounts::default());
        };
        Ok(StateCounts {
            observed_repos: query_count(
                conn,
                "SELECT COUNT(*) FROM observed_repositories WHERE active = 1",
            )?,
            instruction_dirs: query_count(
                conn,
                "SELECT COUNT(*) FROM observed_instruction_dirs d JOIN observed_repositories r ON r.id = d.repository_id WHERE r.active = 1 AND d.active = 1",
            )?,
            migration_candidates: query_count(
                conn,
                "SELECT COUNT(*) FROM detected_claude_files f JOIN observed_repositories r ON r.id = f.repository_id WHERE r.active = 1 AND f.migrated_at IS NULL AND f.classification IN ('user_file', 'tracked_user_file')",
            )?,
            recent_errors: query_count(
                conn,
                "SELECT COUNT(*) FROM events WHERE level = 'error' AND occurred_at >= datetime('now', '-7 days')",
            )?,
            last_reconciled_at: conn
                .query_row(
                    "SELECT MAX(last_reconciled_at) FROM observed_instruction_dirs",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .flatten(),
        })
    }

    pub fn mark_instruction_result(
        &self,
        instruction_dir: &Path,
        status: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            r#"
            UPDATE observed_instruction_dirs
            SET last_reconciled_at = CURRENT_TIMESTAMP,
                last_status = ?2,
                last_error = ?3
            WHERE instruction_dir = ?1
            "#,
            params![instruction_dir.to_string_lossy(), status, error],
        )?;
        conn.execute(
            r#"
            UPDATE observed_repositories
            SET last_reconciled_at = CURRENT_TIMESTAMP,
                last_status = ?2,
                last_error = ?3
            WHERE id IN (
              SELECT repository_id FROM observed_instruction_dirs WHERE instruction_dir = ?1
            )
            "#,
            params![instruction_dir.to_string_lossy(), status, error],
        )?;
        Ok(())
    }

    pub fn mark_shim_removed(&self, shim_id: i64) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            "UPDATE shims SET removed_at = CURRENT_TIMESTAMP WHERE id = ?1 AND removed_at IS NULL",
            params![shim_id],
        )?;
        Ok(())
    }

    pub fn record_event(
        &self,
        level: &str,
        repository_path: Option<&Path>,
        adapter_name: Option<&str>,
        action: &str,
        message: &str,
    ) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };
        conn.execute(
            r#"
            INSERT INTO events
              (occurred_at, level, repository_path, adapter_name, action, message)
            VALUES (CURRENT_TIMESTAMP, ?1, ?2, ?3, ?4, ?5)
            "#,
            params![
                level,
                repository_path.map(|path| path.to_string_lossy().to_string()),
                adapter_name,
                action,
                message,
            ],
        )?;
        Ok(())
    }

    fn ensure_repository(
        &self,
        repo: &GitRepo,
        source: &str,
        observed_cwd: Option<&Path>,
    ) -> Result<i64> {
        let conn = self.conn.as_ref().expect("checked by caller");
        conn.execute(
            r#"
            INSERT INTO observed_repositories
              (root_path, git_dir, git_common_dir, exclude_path, source, first_seen_at, last_seen_at,
               last_observed_cwd, last_reconciled_at, last_status, last_error, active)
            VALUES (?1, ?2, ?3, ?4, ?5, CURRENT_TIMESTAMP, CURRENT_TIMESTAMP,
                    ?6, NULL, NULL, NULL, 1)
            ON CONFLICT(root_path) DO UPDATE SET
              git_dir = excluded.git_dir,
              git_common_dir = excluded.git_common_dir,
              exclude_path = excluded.exclude_path,
              source = excluded.source,
              last_seen_at = CURRENT_TIMESTAMP,
              last_observed_cwd = COALESCE(excluded.last_observed_cwd, last_observed_cwd),
              active = 1
            "#,
            params![
                repo.root.to_string_lossy(),
                repo.git_dir.to_string_lossy(),
                repo.git_common_dir.to_string_lossy(),
                repo.exclude_path.to_string_lossy(),
                source,
                observed_cwd.map(|path| path.to_string_lossy().to_string()),
            ],
        )?;
        conn.query_row(
            "SELECT id FROM observed_repositories WHERE root_path = ?1",
            params![repo.root.to_string_lossy()],
            |row| row.get(0),
        )
        .map_err(Into::into)
    }

    fn migrate(&self) -> Result<()> {
        let Some(conn) = &self.conn else {
            return Ok(());
        };

        conn.execute_batch(
            r#"
            PRAGMA foreign_keys = ON;

            CREATE TABLE IF NOT EXISTS observed_repositories (
              id INTEGER PRIMARY KEY,
              root_path TEXT NOT NULL UNIQUE,
              git_dir TEXT NOT NULL,
              git_common_dir TEXT NOT NULL,
              exclude_path TEXT NOT NULL,
              source TEXT NOT NULL,
              first_seen_at TEXT NOT NULL,
              last_seen_at TEXT NOT NULL,
              last_observed_cwd TEXT,
              last_reconciled_at TEXT,
              last_status TEXT,
              last_error TEXT,
              active INTEGER NOT NULL DEFAULT 1
            );

            CREATE TABLE IF NOT EXISTS observed_instruction_dirs (
              id INTEGER PRIMARY KEY,
              repository_id INTEGER NOT NULL,
              instruction_dir TEXT NOT NULL,
              source_rel_path TEXT NOT NULL,
              target_rel_path TEXT NOT NULL,
              first_seen_at TEXT NOT NULL,
              last_seen_at TEXT NOT NULL,
              last_reconciled_at TEXT,
              last_status TEXT,
              last_error TEXT,
              active INTEGER NOT NULL DEFAULT 1,
              UNIQUE(repository_id, instruction_dir),
              FOREIGN KEY(repository_id) REFERENCES observed_repositories(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS detected_claude_files (
              id INTEGER PRIMARY KEY,
              repository_id INTEGER NOT NULL,
              instruction_dir TEXT NOT NULL,
              claude_path TEXT NOT NULL,
              agents_path TEXT NOT NULL,
              classification TEXT NOT NULL,
              content_hash TEXT,
              first_seen_at TEXT NOT NULL,
              last_seen_at TEXT NOT NULL,
              last_migration_status TEXT,
              last_error TEXT,
              migrated_at TEXT,
              UNIQUE(repository_id, claude_path),
              FOREIGN KEY(repository_id) REFERENCES observed_repositories(id) ON DELETE CASCADE
            );

            CREATE TABLE IF NOT EXISTS shims (
              id INTEGER PRIMARY KEY,
              instruction_dir_id INTEGER NOT NULL,
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
              removed_at TEXT,
              UNIQUE(instruction_dir_id, adapter_name, target_rel_path),
              FOREIGN KEY(instruction_dir_id) REFERENCES observed_instruction_dirs(id) ON DELETE CASCADE
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

            CREATE TABLE IF NOT EXISTS settings (
              key TEXT PRIMARY KEY,
              value TEXT NOT NULL,
              updated_at TEXT NOT NULL
            );

            PRAGMA user_version = 2;
            "#,
        )?;
        Ok(())
    }
}

fn archive_old_state_if_needed(db_path: &Path) -> Result<()> {
    if !db_path.exists() {
        return Ok(());
    }
    let conn = Connection::open(db_path)
        .with_context(|| format!("failed to inspect state db {}", db_path.display()))?;
    let version = user_version(&conn)?;
    let table_count = query_count(
        &conn,
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )?;
    drop(conn);

    if version == STATE_VERSION || table_count == 0 {
        return Ok(());
    }

    let archive = archive_path(db_path)?;
    fs::rename(db_path, &archive).with_context(|| {
        format!(
            "failed to archive old state db {} to {}",
            db_path.display(),
            archive.display()
        )
    })?;
    Ok(())
}

fn archive_path(db_path: &Path) -> Result<PathBuf> {
    let parent = db_path
        .parent()
        .with_context(|| format!("state db {} has no parent", db_path.display()))?;
    let preferred = parent.join(CUTOVER_ARCHIVE);
    if !preferred.exists() {
        return Ok(preferred);
    }
    let suffix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(parent.join(format!("state.pre-observed-cutover.{suffix}.sqlite3")))
}

fn user_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(Into::into)
}

fn query_count(conn: &Connection, sql: &str) -> Result<usize> {
    let count: i64 = conn.query_row(sql, [], |row| row.get(0))?;
    Ok(count as usize)
}
