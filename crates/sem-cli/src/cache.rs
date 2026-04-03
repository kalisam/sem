use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection};
use sem_core::model::entity::SemanticEntity;
use sem_core::parser::graph::{EntityGraph, EntityInfo, EntityRef, RefType};

pub struct DiskCache {
    conn: Connection,
}

impl DiskCache {
    pub fn open(repo_root: &Path) -> Result<Self, rusqlite::Error> {
        let cache_dir = repo_root.join(".sem");
        std::fs::create_dir_all(&cache_dir).ok();
        let db_path = cache_dir.join("cache.db");
        let conn = Connection::open(db_path)?;

        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS files (
                 path TEXT PRIMARY KEY,
                 mtime_secs INTEGER NOT NULL,
                 mtime_nanos INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS entities (
                 id TEXT PRIMARY KEY,
                 name TEXT NOT NULL,
                 entity_type TEXT NOT NULL,
                 file_path TEXT NOT NULL,
                 start_line INTEGER NOT NULL,
                 end_line INTEGER NOT NULL,
                 content TEXT NOT NULL,
                 content_hash TEXT NOT NULL,
                 structural_hash TEXT,
                 parent_id TEXT,
                 metadata_json TEXT
             );
             CREATE TABLE IF NOT EXISTS edges (
                 from_entity TEXT NOT NULL,
                 to_entity TEXT NOT NULL,
                 ref_type TEXT NOT NULL
             );",
        )?;

        Ok(Self { conn })
    }

    pub fn save(
        &self,
        root: &Path,
        files: &[String],
        graph: &EntityGraph,
        entities: &[SemanticEntity],
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;

        tx.execute_batch("DELETE FROM files; DELETE FROM entities; DELETE FROM edges;")?;

        {
            let mut stmt = tx.prepare(
                "INSERT INTO files (path, mtime_secs, mtime_nanos) VALUES (?1, ?2, ?3)",
            )?;
            for file in files {
                let full = root.join(file);
                if let Ok(meta) = std::fs::metadata(&full) {
                    if let Ok(mtime) = meta.modified() {
                        let dur = mtime
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default();
                        stmt.execute(params![file, dur.as_secs() as i64, dur.subsec_nanos() as i64])?;
                    }
                }
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO entities (id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            for e in entities {
                let metadata_json = e
                    .metadata
                    .as_ref()
                    .and_then(|m| serde_json::to_string(m).ok());
                stmt.execute(params![
                    e.id,
                    e.name,
                    e.entity_type,
                    e.file_path,
                    e.start_line as i64,
                    e.end_line as i64,
                    e.content,
                    e.content_hash,
                    e.structural_hash,
                    e.parent_id,
                    metadata_json,
                ])?;
            }
        }

        {
            let mut stmt = tx.prepare(
                "INSERT INTO edges (from_entity, to_entity, ref_type) VALUES (?1, ?2, ?3)",
            )?;
            for edge in &graph.edges {
                let rt = match edge.ref_type {
                    RefType::Calls => "calls",
                    RefType::TypeRef => "typeref",
                    RefType::Imports => "imports",
                };
                stmt.execute(params![edge.from_entity, edge.to_entity, rt])?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    pub fn load(
        &self,
        root: &Path,
        files: &[String],
    ) -> Option<(EntityGraph, Vec<SemanticEntity>)> {
        let cached_count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
            .ok()?;
        if cached_count as usize != files.len() {
            return None;
        }

        let mut stmt = self
            .conn
            .prepare("SELECT mtime_secs, mtime_nanos FROM files WHERE path = ?1")
            .ok()?;
        for file in files {
            let full = root.join(file);
            let meta = std::fs::metadata(&full).ok()?;
            let mtime = meta.modified().ok()?;
            let dur = mtime
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default();

            let (secs, nanos): (i64, i64) = stmt
                .query_row(params![file], |row| Ok((row.get(0)?, row.get(1)?)))
                .ok()?;
            if secs != dur.as_secs() as i64 || nanos != dur.subsec_nanos() as i64 {
                return None;
            }
        }

        let mut entity_stmt = self
            .conn
            .prepare("SELECT id, name, entity_type, file_path, start_line, end_line, content, content_hash, structural_hash, parent_id, metadata_json FROM entities")
            .ok()?;
        let entities: Vec<SemanticEntity> = entity_stmt
            .query_map([], |row| {
                let metadata_json: Option<String> = row.get(10)?;
                let metadata = metadata_json.and_then(|j| serde_json::from_str(&j).ok());
                Ok(SemanticEntity {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    entity_type: row.get(2)?,
                    file_path: row.get(3)?,
                    start_line: row.get::<_, i64>(4)? as usize,
                    end_line: row.get::<_, i64>(5)? as usize,
                    content: row.get(6)?,
                    content_hash: row.get(7)?,
                    structural_hash: row.get(8)?,
                    parent_id: row.get(9)?,
                    metadata,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let mut edge_stmt = self
            .conn
            .prepare("SELECT from_entity, to_entity, ref_type FROM edges")
            .ok()?;
        let edges: Vec<EntityRef> = edge_stmt
            .query_map([], |row| {
                let rt: String = row.get(2)?;
                let ref_type = match rt.as_str() {
                    "calls" => RefType::Calls,
                    "imports" => RefType::Imports,
                    _ => RefType::TypeRef,
                };
                Ok(EntityRef {
                    from_entity: row.get(0)?,
                    to_entity: row.get(1)?,
                    ref_type,
                })
            })
            .ok()?
            .filter_map(|r| r.ok())
            .collect();

        let entity_map: HashMap<String, EntityInfo> = entities
            .iter()
            .map(|e| {
                (
                    e.id.clone(),
                    EntityInfo {
                        id: e.id.clone(),
                        name: e.name.clone(),
                        entity_type: e.entity_type.clone(),
                        file_path: e.file_path.clone(),
                        start_line: e.start_line,
                        end_line: e.end_line,
                    },
                )
            })
            .collect();

        let graph = EntityGraph::from_parts(entity_map, edges);
        Some((graph, entities))
    }
}
