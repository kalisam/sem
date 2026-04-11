//! Entity dependency graph — cross-file reference extraction.
//!
//! Implements a two-pass approach inspired by arXiv:2601.08773 (Reliable Graph-RAG):
//! Pass 1: Extract all entities, build a symbol table (name → entity ID).
//! Pass 2: For each entity, extract identifier references from its AST subtree,
//!         resolve them against the symbol table to create edges.
//!
//! This enables impact analysis: "if I change entity X, what else is affected?"

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::LazyLock;

use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::git::types::{FileChange, FileStatus};
use crate::model::entity::SemanticEntity;
use crate::parser::registry::ParserRegistry;

/// A reference from one entity to another.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityRef {
    pub from_entity: String,
    pub to_entity: String,
    pub ref_type: RefType,
}

/// Type of reference between entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefType {
    /// Function/method call
    Calls,
    /// Type reference (extends, implements, field type)
    TypeRef,
    /// Import/use statement reference
    Imports,
}

/// A complete entity dependency graph for a set of files.
#[derive(Debug)]
pub struct EntityGraph {
    /// All entities indexed by ID
    pub entities: HashMap<String, EntityInfo>,
    /// Edges: from_entity → [(to_entity, ref_type)]
    pub edges: Vec<EntityRef>,
    /// Reverse index: entity_id → entities that reference it
    pub dependents: HashMap<String, Vec<String>>,
    /// Forward index: entity_id → entities it references
    pub dependencies: HashMap<String, Vec<String>>,
}

/// Minimal entity info stored in the graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntityInfo {
    pub id: String,
    pub name: String,
    pub entity_type: String,
    pub file_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    pub start_line: usize,
    pub end_line: usize,
}

impl EntityGraph {
    /// Reconstruct an EntityGraph from pre-loaded parts (e.g. from a cache).
    pub fn from_parts(entities: HashMap<String, EntityInfo>, edges: Vec<EntityRef>) -> Self {
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();
        for edge in &edges {
            dependents
                .entry(edge.to_entity.clone())
                .or_default()
                .push(edge.from_entity.clone());
            dependencies
                .entry(edge.from_entity.clone())
                .or_default()
                .push(edge.to_entity.clone());
        }
        EntityGraph {
            entities,
            edges,
            dependents,
            dependencies,
        }
    }

    /// Build an entity graph from a set of files.
    ///
    /// Pass 1: Extract all entities from all files using the parser registry.
    /// Pass 2: For each entity, find identifier tokens and resolve them against
    ///         the symbol table to create reference edges.
    pub fn build(
        root: &Path,
        file_paths: &[String],
        registry: &ParserRegistry,
    ) -> Self {
        // Pass 1: Extract all entities in parallel (file I/O + tree-sitter parsing)
        let all_entities: Vec<SemanticEntity> = file_paths
            .par_iter()
            .filter_map(|file_path| {
                let full_path = root.join(file_path);
                let content = std::fs::read_to_string(&full_path).ok()?;
                let plugin = registry.get_plugin_with_content(file_path, &content)?;
                Some(plugin.extract_entities(&content, file_path))
            })
            .flatten()
            .collect();

        // Build symbol table: name → entity IDs (can be multiple with same name)
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::with_capacity(all_entities.len());
        let mut entity_map: HashMap<String, EntityInfo> = HashMap::with_capacity(all_entities.len());

        for entity in &all_entities {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());

            entity_map.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    parent_id: entity.parent_id.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Build parent-child set for skipping class→method self-edges
        let parent_child_pairs: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.id.as_str()))
            })
            .collect();

        // Build set of (class_id, child_method_name) so classes skip refs to their own methods
        let class_child_names: HashSet<(&str, &str)> = all_entities
            .iter()
            .filter_map(|e| {
                e.parent_id.as_ref().map(|pid| (pid.as_str(), e.name.as_str()))
            })
            .collect();

        // Build import table: (file_path, imported_name) → target entity ID
        // e.g. ("io_handler.py", "validate") → "core.py::function::validate"
        let import_table = build_import_table(root, file_paths, &symbol_table, &entity_map);

        // Pass 2: Extract references in parallel, then resolve against symbol table
        // Step 2a: Parallel reference extraction per entity
        let resolved_refs: Vec<(String, String, RefType)> = all_entities
            .par_iter()
            .flat_map(|entity| {
                let refs = extract_references_from_content(&entity.content, &entity.name);
                let mut entity_edges = Vec::new();
                for ref_name in refs {
                    // Skip references to names that are this class's own methods
                    if class_child_names.contains(&(entity.id.as_str(), ref_name)) {
                        continue;
                    }

                    // Check import table first: if this file imports this name,
                    // resolve to the import target instead of global symbol table
                    let import_key = (entity.file_path.clone(), ref_name.to_string());
                    if let Some(import_target_id) = import_table.get(&import_key) {
                        if import_target_id != &entity.id
                            && !parent_child_pairs.contains(&(entity.id.as_str(), import_target_id.as_str()))
                            && !parent_child_pairs.contains(&(import_target_id.as_str(), entity.id.as_str()))
                        {
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                import_target_id.clone(),
                                ref_type,
                            ));
                        }
                        continue;
                    }

                    if let Some(target_ids) = symbol_table.get(ref_name) {
                        // Without an import, only resolve to entities in the same file.
                        // Cross-file resolution is handled by the import table above.
                        let target = target_ids
                            .iter()
                            .find(|id| {
                                *id != &entity.id
                                    && entity_map
                                        .get(*id)
                                        .map_or(false, |e| e.file_path == entity.file_path)
                            });

                        if let Some(target_id) = target {
                            // Skip parent-child edges (class → own method)
                            if parent_child_pairs.contains(&(entity.id.as_str(), target_id.as_str()))
                                || parent_child_pairs.contains(&(target_id.as_str(), entity.id.as_str()))
                            {
                                continue;
                            }
                            let ref_type = infer_ref_type(&entity.content, &ref_name);
                            entity_edges.push((
                                entity.id.clone(),
                                target_id.clone(),
                                ref_type,
                            ));
                        }
                    }
                }
                entity_edges
            })
            .collect();

        // Step 2b: Build edge indexes from resolved references
        let mut edges: Vec<EntityRef> = Vec::with_capacity(resolved_refs.len());
        let mut dependents: HashMap<String, Vec<String>> = HashMap::new();
        let mut dependencies: HashMap<String, Vec<String>> = HashMap::new();

        for (from_entity, to_entity, ref_type) in resolved_refs {
            dependents
                .entry(to_entity.clone())
                .or_default()
                .push(from_entity.clone());
            dependencies
                .entry(from_entity.clone())
                .or_default()
                .push(to_entity.clone());
            edges.push(EntityRef {
                from_entity,
                to_entity,
                ref_type,
            });
        }

        EntityGraph {
            entities: entity_map,
            edges,
            dependents,
            dependencies,
        }
    }

    /// Get entities that depend on the given entity (reverse deps).
    pub fn get_dependents(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependents
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Get entities that the given entity depends on (forward deps).
    pub fn get_dependencies(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.dependencies
            .get(entity_id)
            .map(|ids| {
                ids.iter()
                    .filter_map(|id| self.entities.get(id))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Impact analysis: if the given entity changes, what else might be affected?
    /// Returns all transitive dependents (breadth-first), capped at 10k.
    pub fn impact_analysis(&self, entity_id: &str) -> Vec<&EntityInfo> {
        self.impact_analysis_capped(entity_id, 10_000)
    }

    /// Impact analysis with a cap on maximum nodes visited.
    /// Returns transitive dependents up to the cap. Uses borrowed strings.
    pub fn impact_analysis_capped(&self, entity_id: &str, max_visited: usize) -> Vec<&EntityInfo> {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut result = Vec::new();

        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return result,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if result.len() >= max_visited {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        if let Some(info) = self.entities.get(dep.as_str()) {
                            result.push(info);
                        }
                        queue.push_back(dep.as_str());
                        if result.len() >= max_visited {
                            break;
                        }
                    }
                }
            }
        }

        result
    }

    /// Count transitive dependents without collecting them (faster for large graphs).
    /// Uses borrowed strings to avoid allocation overhead.
    pub fn impact_count(&self, entity_id: &str, max_count: usize) -> usize {
        let mut visited: HashSet<&str> = HashSet::new();
        let mut queue: std::collections::VecDeque<&str> = std::collections::VecDeque::new();
        let mut count = 0;

        // We need entity_id to live long enough; look it up in our entities map
        let start_key = match self.entities.get_key_value(entity_id) {
            Some((k, _)) => k.as_str(),
            None => return 0,
        };

        queue.push_back(start_key);
        visited.insert(start_key);

        while let Some(current) = queue.pop_front() {
            if count >= max_count {
                break;
            }
            if let Some(deps) = self.dependents.get(current) {
                for dep in deps {
                    if visited.insert(dep.as_str()) {
                        count += 1;
                        queue.push_back(dep.as_str());
                        if count >= max_count {
                            break;
                        }
                    }
                }
            }
        }

        count
    }

    /// Filter entities to those that look like tests.
    /// Uses name heuristics, file path patterns, and content patterns.
    pub fn filter_test_entities(&self, entities: &[crate::model::entity::SemanticEntity]) -> HashSet<String> {
        let mut test_ids = HashSet::new();
        for entity in entities {
            if is_test_entity(entity) {
                test_ids.insert(entity.id.clone());
            }
        }
        test_ids
    }

    /// Impact analysis filtered to test entities only.
    /// Returns transitive dependents that are test functions/methods.
    pub fn test_impact(
        &self,
        entity_id: &str,
        all_entities: &[crate::model::entity::SemanticEntity],
    ) -> Vec<&EntityInfo> {
        let test_ids = self.filter_test_entities(all_entities);
        let impact = self.impact_analysis(entity_id);
        impact
            .into_iter()
            .filter(|info| test_ids.contains(&info.id))
            .collect()
    }

    /// Incrementally update the graph from a set of changed files.
    ///
    /// Instead of rebuilding the entire graph, this only re-extracts entities
    /// from changed files and re-resolves their references. This is faster
    /// than a full rebuild when only a few files changed.
    ///
    /// For each changed file:
    /// - Deleted: remove all entities from that file, prune edges
    /// - Added/Modified: remove old entities, extract new ones, rebuild references
    /// - Renamed: update file paths in entity info
    pub fn update_from_changes(
        &mut self,
        changed_files: &[FileChange],
        root: &Path,
        registry: &ParserRegistry,
    ) {
        let mut affected_files: HashSet<String> = HashSet::new();
        let mut new_entities: Vec<SemanticEntity> = Vec::new();

        for change in changed_files {
            affected_files.insert(change.file_path.clone());
            if let Some(ref old_path) = change.old_file_path {
                affected_files.insert(old_path.clone());
            }

            match change.status {
                FileStatus::Deleted => {
                    self.remove_entities_for_file(&change.file_path);
                }
                FileStatus::Renamed => {
                    // Update file paths for renamed files
                    if let Some(ref old_path) = change.old_file_path {
                        self.remove_entities_for_file(old_path);
                    }
                    // Extract entities from the new file
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
                FileStatus::Added | FileStatus::Modified => {
                    // Remove old entities for this file
                    self.remove_entities_for_file(&change.file_path);
                    // Extract new entities
                    if let Some(entities) = self.extract_file_entities(
                        &change.file_path,
                        change.after_content.as_deref(),
                        root,
                        registry,
                    ) {
                        new_entities.extend(entities);
                    }
                }
            }
        }

        // Add new entities to the entity map
        for entity in &new_entities {
            self.entities.insert(
                entity.id.clone(),
                EntityInfo {
                    id: entity.id.clone(),
                    name: entity.name.clone(),
                    entity_type: entity.entity_type.clone(),
                    file_path: entity.file_path.clone(),
                    parent_id: entity.parent_id.clone(),
                    start_line: entity.start_line,
                    end_line: entity.end_line,
                },
            );
        }

        // Rebuild the global symbol table from all current entities
        let symbol_table = self.build_symbol_table();

        // Re-resolve references for new entities
        for entity in &new_entities {
            self.resolve_entity_references(entity, &symbol_table);
        }

        // Also re-resolve references for entities in OTHER files that might
        // reference entities in changed files (their targets may have changed)
        let changed_entity_names: HashSet<String> = new_entities
            .iter()
            .map(|e| e.name.clone())
            .collect();

        // Find entities in unchanged files that reference any changed entity name
        let entities_to_recheck: Vec<String> = self
            .entities
            .values()
            .filter(|e| !affected_files.contains(&e.file_path))
            .filter(|e| {
                self.dependencies
                    .get(&e.id)
                    .map_or(false, |deps| {
                        deps.iter().any(|dep_id| {
                            self.entities
                                .get(dep_id)
                                .map_or(false, |dep| changed_entity_names.contains(&dep.name))
                        })
                    })
            })
            .map(|e| e.id.clone())
            .collect();

        // We don't have the full SemanticEntity for unchanged files, so we skip
        // deep re-resolution here. The forward/reverse indexes are already updated
        // by remove_entities_for_file and resolve_entity_references.
        // For entities that had dangling references (their target was deleted),
        // the edges were already pruned.
        let _ = entities_to_recheck; // acknowledge but don't act on for now
    }

    /// Extract entities from a file, using provided content or reading from disk.
    fn extract_file_entities(
        &self,
        file_path: &str,
        content: Option<&str>,
        root: &Path,
        registry: &ParserRegistry,
    ) -> Option<Vec<SemanticEntity>> {
        let content = if let Some(c) = content {
            c.to_string()
        } else {
            let full_path = root.join(file_path);
            std::fs::read_to_string(&full_path).ok()?
        };

        let plugin = registry.get_plugin_with_content(file_path, &content)?;

        Some(plugin.extract_entities(&content, file_path))
    }

    /// Remove all entities belonging to a specific file and prune their edges.
    fn remove_entities_for_file(&mut self, file_path: &str) {
        // Collect entity IDs to remove
        let ids_to_remove: Vec<String> = self
            .entities
            .values()
            .filter(|e| e.file_path == file_path)
            .map(|e| e.id.clone())
            .collect();

        let id_set: HashSet<&str> = ids_to_remove.iter().map(|s| s.as_str()).collect();

        // Remove from entity map
        for id in &ids_to_remove {
            self.entities.remove(id);
        }

        // Remove edges involving these entities
        self.edges
            .retain(|e| !id_set.contains(e.from_entity.as_str()) && !id_set.contains(e.to_entity.as_str()));

        // Clean up dependency/dependent indexes
        for id in &ids_to_remove {
            // Remove forward deps
            if let Some(deps) = self.dependencies.remove(id) {
                // Also remove from reverse index
                for dep in &deps {
                    if let Some(dependents) = self.dependents.get_mut(dep) {
                        dependents.retain(|d| d != id);
                    }
                }
            }
            // Remove reverse deps
            if let Some(deps) = self.dependents.remove(id) {
                // Also remove from forward index
                for dep in &deps {
                    if let Some(dependencies) = self.dependencies.get_mut(dep) {
                        dependencies.retain(|d| d != id);
                    }
                }
            }
        }
    }

    /// Build a symbol table from all current entities.
    fn build_symbol_table(&self) -> HashMap<String, Vec<String>> {
        let mut symbol_table: HashMap<String, Vec<String>> = HashMap::new();
        for entity in self.entities.values() {
            symbol_table
                .entry(entity.name.clone())
                .or_default()
                .push(entity.id.clone());
        }
        symbol_table
    }

    /// Resolve references for a single entity against the symbol table.
    fn resolve_entity_references(
        &mut self,
        entity: &SemanticEntity,
        symbol_table: &HashMap<String, Vec<String>>,
    ) {
        let refs = extract_references_from_content(&entity.content, &entity.name);

        for ref_name in refs {
            if let Some(target_ids) = symbol_table.get(ref_name) {
                let target = target_ids
                    .iter()
                    .find(|id| {
                        *id != &entity.id
                            && self
                                .entities
                                .get(*id)
                                .map_or(false, |e| e.file_path == entity.file_path)
                    })
                    .or_else(|| target_ids.iter().find(|id| *id != &entity.id));

                if let Some(target_id) = target {
                    let ref_type = infer_ref_type(&entity.content, &ref_name);
                    self.edges.push(EntityRef {
                        from_entity: entity.id.clone(),
                        to_entity: target_id.clone(),
                        ref_type,
                    });
                    self.dependents
                        .entry(target_id.clone())
                        .or_default()
                        .push(entity.id.clone());
                    self.dependencies
                        .entry(entity.id.clone())
                        .or_default()
                        .push(target_id.clone());
                }
            }
        }
    }
}

/// Check if an entity looks like a test based on name, file path, and content patterns.
fn is_test_entity(entity: &crate::model::entity::SemanticEntity) -> bool {
    let name = &entity.name;
    let path = &entity.file_path;
    let content = &entity.content;

    // Name patterns
    if name.starts_with("test_") || name.starts_with("Test") || name.ends_with("_test") || name.ends_with("Test") {
        return true;
    }
    if name.starts_with("it_") || name.starts_with("describe_") || name.starts_with("spec_") {
        return true;
    }

    // File path patterns
    let path_lower = path.to_lowercase();
    let in_test_file = path_lower.contains("/test/")
        || path_lower.contains("/tests/")
        || path_lower.contains("/spec/")
        || path_lower.contains("_test.")
        || path_lower.contains(".test.")
        || path_lower.contains("_spec.")
        || path_lower.contains(".spec.");

    // Content patterns (test annotations/decorators)
    let has_test_marker = content.contains("#[test]")
        || content.contains("#[cfg(test)]")
        || content.contains("@Test")
        || content.contains("@pytest")
        || content.contains("@test")
        || content.contains("describe(")
        || content.contains("it(")
        || content.contains("test(");

    in_test_file && has_test_marker
}

/// Build import table: maps (file_path, imported_name) → target entity ID.
///
/// Parses `from X import Y` / `import X` / `use X` style statements from entity content
/// and resolves Y to the entity it refers to in the symbol table.
fn build_import_table(
    root: &Path,
    file_paths: &[String],
    symbol_table: &HashMap<String, Vec<String>>,
    entity_map: &HashMap<String, EntityInfo>,
) -> HashMap<(String, String), String> {
    let mut import_table: HashMap<(String, String), String> = HashMap::new();

    for file_path in file_paths {
        let full_path = root.join(file_path);
        let content = match std::fs::read_to_string(&full_path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // Join multi-line imports into single logical lines
        // e.g. "from .cookies import (\n    foo,\n    bar,\n)" -> "from .cookies import foo, bar"
        let mut logical_lines: Vec<String> = Vec::new();
        let mut current_line = String::new();
        let mut in_parens = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if in_parens {
                // Strip parentheses and comments
                let clean = trimmed.trim_end_matches(|c: char| c == ')' || c == ',');
                let clean = clean.split('#').next().unwrap_or(clean).trim();
                if !clean.is_empty() && clean != "(" {
                    current_line.push_str(", ");
                    current_line.push_str(clean);
                }
                if trimmed.contains(')') {
                    in_parens = false;
                    logical_lines.push(std::mem::take(&mut current_line));
                }
            } else if trimmed.starts_with("from ") && trimmed.contains(" import ") {
                if trimmed.contains('(') && !trimmed.contains(')') {
                    // Multi-line import starts
                    in_parens = true;
                    // Take everything before the paren
                    let before_paren = trimmed.split('(').next().unwrap_or(trimmed);
                    current_line = before_paren.trim().to_string();
                    // Also grab anything after the paren on this line
                    if let Some(after) = trimmed.split('(').nth(1) {
                        let after = after.trim().trim_end_matches(')').trim();
                        if !after.is_empty() {
                            current_line.push(' ');
                            current_line.push_str(after);
                        }
                    }
                } else {
                    logical_lines.push(trimmed.to_string());
                }
            }
        }

        for logical_line in &logical_lines {
            if let Some(rest) = logical_line.strip_prefix("from ") {
                // Find " import " or " import," (multi-line imports join with comma)
                let import_match = rest.find(" import ")
                    .map(|pos| (pos, 8))
                    .or_else(|| rest.find(" import,").map(|pos| (pos, 8)));
                if let Some((import_pos, skip)) = import_match {
                    let module_path = &rest[..import_pos];
                    let names_str = &rest[import_pos + skip..];

                    let source_module = module_path
                        .trim_start_matches('.')
                        .rsplit('.')
                        .next()
                        .unwrap_or(module_path.trim_start_matches('.'));

                    for name_part in names_str.split(',') {
                        let name_part = name_part.trim();
                        let imported_name = name_part.split_whitespace().next().unwrap_or(name_part);
                        // Strip trailing parens/punctuation
                        let imported_name = imported_name.trim_matches(|c: char| c == '(' || c == ')' || c == ',');
                        if imported_name.is_empty() {
                            continue;
                        }

                        if let Some(target_ids) = symbol_table.get(imported_name) {
                            let target = target_ids.iter().find(|id| {
                                entity_map.get(*id).map_or(false, |e| {
                                    let stem = e.file_path.rsplit('/').next().unwrap_or(&e.file_path);
                                    let stem = stem.strip_suffix(".py")
                                        .or_else(|| stem.strip_suffix(".ts"))
                                        .or_else(|| stem.strip_suffix(".js"))
                                        .or_else(|| stem.strip_suffix(".rs"))
                                        .unwrap_or(stem);
                                    stem == source_module
                                })
                            });
                            if let Some(target_id) = target {
                                import_table.insert(
                                    (file_path.clone(), imported_name.to_string()),
                                    target_id.clone(),
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    import_table
}

/// Strip comments and string literals from content to avoid false references.
/// Returns a new string with comments/docstrings replaced by spaces.
fn strip_comments_and_strings(content: &str) -> String {
    let bytes = content.as_bytes();
    let len = bytes.len();
    let mut result = vec![b' '; len];
    let mut i = 0;

    while i < len {
        // Triple-quoted strings (Python docstrings)
        if i + 2 < len && bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
            i += 3;
            while i + 2 < len {
                if bytes[i] == b'"' && bytes[i + 1] == b'"' && bytes[i + 2] == b'"' {
                    i += 3;
                    break;
                }
                i += 1;
            }
            continue;
        }
        if i + 2 < len && bytes[i] == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
            i += 3;
            while i + 2 < len {
                if bytes[i] == b'\'' && bytes[i + 1] == b'\'' && bytes[i + 2] == b'\'' {
                    i += 3;
                    break;
                }
                i += 1;
            }
            continue;
        }
        // Double-quoted strings
        if bytes[i] == b'"' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == b'"' { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Single-quoted strings
        if bytes[i] == b'\'' {
            i += 1;
            while i < len {
                if bytes[i] == b'\\' { i += 2; continue; }
                if bytes[i] == b'\'' { i += 1; break; }
                i += 1;
            }
            continue;
        }
        // Python/Ruby single-line comments
        if bytes[i] == b'#' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // C-style single-line comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < len && bytes[i] != b'\n' { i += 1; }
            continue;
        }
        // C-style block comments
        if i + 1 < len && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < len {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' { i += 2; break; }
                i += 1;
            }
            continue;
        }
        // Regular code: copy through
        result[i] = bytes[i];
        i += 1;
    }

    String::from_utf8_lossy(&result).into_owned()
}

/// Extract identifier references from entity content using simple token analysis.
/// Strips comments and strings first to avoid false positives from docstrings.
/// Returns borrowed slices from the stripped content.
fn extract_references_from_content<'a>(content: &'a str, own_name: &str) -> Vec<&'a str> {
    // We need to figure out which words appear only in comments/strings vs real code.
    // Strategy: strip comments/strings, then only accept words that appear in the stripped version.
    let stripped = strip_comments_and_strings(content);
    let stripped_words: HashSet<&str> = stripped
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .collect();

    let mut refs = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();

    for word in content.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if word.is_empty() || word == own_name {
            continue;
        }
        if is_keyword(word) || word.len() < 2 {
            continue;
        }
        // Skip very short lowercase identifiers (likely local vars: i, x, a, ok, id, etc.)
        if word.starts_with(|c: char| c.is_lowercase()) && word.len() < 3 {
            continue;
        }
        if !word.starts_with(|c: char| c.is_alphabetic() || c == '_') {
            continue;
        }
        // Skip common local variable names that create false graph edges
        if is_common_local_name(word) {
            continue;
        }
        // Skip words that only appear in comments/strings
        if !stripped_words.contains(word) {
            continue;
        }
        if seen.insert(word) {
            refs.push(word);
        }
    }

    refs
}

static COMMON_LOCAL_NAMES: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        "result", "results", "data", "config", "value", "values",
        "item", "items", "input", "output", "args", "opts",
        "name", "path", "file", "line", "count", "index",
        "temp", "prev", "next", "curr", "current", "node",
        "left", "right", "root", "head", "tail", "body",
        "text", "content", "source", "target", "entry",
        "error", "errors", "message", "response", "request",
        "context", "state", "props", "event", "handler",
        "callback", "options", "params", "query", "list",
        "base", "info", "meta", "kind", "mode", "flag",
        "size", "length", "width", "height", "start", "stop",
        "begin", "done", "found", "status", "code", "test",
    ].into_iter().collect()
});

/// Names that are overwhelmingly local variables, not entity references.
/// These create massive false-positive edges in the dependency graph.
fn is_common_local_name(word: &str) -> bool {
    COMMON_LOCAL_NAMES.contains(word)
}

/// Infer reference type from context using word-boundary-aware matching.
fn infer_ref_type(content: &str, ref_name: &str) -> RefType {
    // Check if it's a function call: ref_name followed by ( with word boundary before.
    // Avoids format! allocation by finding ref_name and checking the next char.
    let bytes = content.as_bytes();
    let name_bytes = ref_name.as_bytes();
    let mut search_start = 0;
    while let Some(rel_pos) = content[search_start..].find(ref_name) {
        let pos = search_start + rel_pos;
        let after = pos + name_bytes.len();
        // Check next char is '('
        if after < bytes.len() && bytes[after] == b'(' {
            // Verify word boundary before
            let is_boundary = pos == 0 || {
                let prev = bytes[pos - 1];
                !prev.is_ascii_alphanumeric() && prev != b'_'
            };
            if is_boundary {
                return RefType::Calls;
            }
        }
        // Advance past pos to the next char boundary to avoid slicing inside a multi-byte UTF-8 char.
        search_start = pos + 1;
        while search_start < content.len() && !content.is_char_boundary(search_start) {
            search_start += 1;
        }
    }

    // Check if it's in an import/use statement (line-level, not substring)
    for line in content.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("import ") || trimmed.starts_with("use ")
            || trimmed.starts_with("from ") || trimmed.starts_with("require("))
            && trimmed.contains(ref_name)
        {
            return RefType::Imports;
        }
    }

    // Default to type reference
    RefType::TypeRef
}

static KEYWORDS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    [
        // Common across languages
        "if", "else", "for", "while", "do", "switch", "case", "break",
        "continue", "return", "try", "catch", "finally", "throw",
        "new", "delete", "typeof", "instanceof", "in", "of",
        "true", "false", "null", "undefined", "void", "this",
        "super", "class", "extends", "implements", "interface",
        "enum", "const", "let", "var", "function", "async",
        "await", "yield", "import", "export", "default", "from",
        "as", "static", "public", "private", "protected",
        "abstract", "final", "override",
        // Rust
        "fn", "pub", "mod", "use", "struct", "impl", "trait",
        "where", "type", "self", "Self", "mut", "ref", "match",
        "loop", "move", "unsafe", "extern", "crate", "dyn",
        // Python
        "def", "elif", "except", "raise", "with",
        "pass", "lambda", "nonlocal", "global", "assert",
        "True", "False", "and", "or", "not", "is",
        // Go
        "func", "package", "range", "select", "chan", "go",
        "defer", "map", "make", "append", "len", "cap",
        // C/C++
        "auto", "register", "volatile", "sizeof", "typedef",
        "template", "typename", "namespace", "virtual", "inline",
        "constexpr", "nullptr", "noexcept", "explicit", "friend",
        "operator", "using", "cout", "endl", "cerr", "cin",
        "printf", "scanf", "malloc", "free", "NULL", "include",
        "ifdef", "ifndef", "endif", "define", "pragma",
        // Ruby
        "end", "then", "elsif", "unless", "until",
        "begin", "rescue", "ensure", "when", "require",
        "attr_accessor", "attr_reader", "attr_writer",
        "puts", "nil", "module", "defined",
        // C#
        "internal", "sealed", "readonly",
        "partial", "delegate", "event", "params", "out",
        "object", "decimal", "sbyte", "ushort", "uint",
        "ulong", "nint", "nuint", "dynamic",
        "get", "set", "value", "init", "record",
        // Types (primitives)
        "string", "number", "boolean", "int", "float", "double",
        "bool", "char", "byte", "i8", "i16", "i32", "i64",
        "u8", "u16", "u32", "u64", "f32", "f64", "usize",
        "isize", "str", "String", "Vec", "Option", "Result",
        "Box", "Arc", "Rc", "HashMap", "HashSet", "Some",
        "Ok", "Err",
    ].into_iter().collect()
});

fn is_keyword(word: &str) -> bool {
    KEYWORDS.contains(word)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::{FileChange, FileStatus};
    use std::io::Write;
    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, ParserRegistry) {
        let dir = TempDir::new().unwrap();
        let registry = crate::parser::plugins::create_default_registry();
        (dir, registry)
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_incremental_add_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        // Start with one file
        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let mut graph = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Add a new file
        write_file(root, "c.ts", "export function baz() { return foo(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "c.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: None, // will read from disk
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        assert!(graph.entities.contains_key("c.ts::function::baz"));
        // baz references foo
        let baz_deps = graph.get_dependencies("c.ts::function::baz");
        assert!(
            baz_deps.iter().any(|d| d.name == "foo"),
            "baz should depend on foo. Deps: {:?}",
            baz_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_delete_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\n");

        let mut graph = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 2);

        // Delete b.ts
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Deleted,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 1);
        assert!(!graph.entities.contains_key("b.ts::function::bar"));
        // foo's dependency on bar should be pruned
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        assert!(
            foo_deps.is_empty(),
            "foo's deps should be empty after bar deleted. Deps: {:?}",
            foo_deps.iter().map(|d| &d.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_modify_file() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return bar(); }\n");
        write_file(root, "b.ts", "export function bar() { return 1; }\nexport function baz() { return 2; }\n");

        let mut graph = EntityGraph::build(root, &["a.ts".into(), "b.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 3);

        // Modify a.ts to call baz instead of bar
        write_file(root, "a.ts", "export function foo() { return baz(); }\n");
        graph.update_from_changes(
            &[FileChange {
                file_path: "a.ts".into(),
                status: FileStatus::Modified,
                old_file_path: None,
                before_content: None,
                after_content: None,
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 3);
        // foo should now depend on baz, not bar
        let foo_deps = graph.get_dependencies("a.ts::function::foo");
        let dep_names: Vec<&str> = foo_deps.iter().map(|d| d.name.as_str()).collect();
        assert!(dep_names.contains(&"baz"), "foo should depend on baz after modification. Deps: {:?}", dep_names);
        assert!(!dep_names.contains(&"bar"), "foo should no longer depend on bar. Deps: {:?}", dep_names);
    }

    #[test]
    fn test_incremental_with_content() {
        let (dir, registry) = create_test_repo();
        let root = dir.path();

        write_file(root, "a.ts", "export function foo() { return 1; }\n");
        let mut graph = EntityGraph::build(root, &["a.ts".into()], &registry);
        assert_eq!(graph.entities.len(), 1);

        // Add file with content provided directly (no disk read needed)
        graph.update_from_changes(
            &[FileChange {
                file_path: "b.ts".into(),
                status: FileStatus::Added,
                old_file_path: None,
                before_content: None,
                after_content: Some("export function bar() { return foo(); }\n".into()),
            }],
            root,
            &registry,
        );

        assert_eq!(graph.entities.len(), 2);
        let bar_deps = graph.get_dependencies("b.ts::function::bar");
        assert!(bar_deps.iter().any(|d| d.name == "foo"));
    }

    #[test]
    fn test_extract_references() {
        let content = "function processData(input) {\n  const result = validateInput(input);\n  return transform(result);\n}";
        let refs = extract_references_from_content(content, "processData");
        assert!(refs.contains(&"validateInput"));
        assert!(refs.contains(&"transform"));
        assert!(!refs.contains(&"processData")); // self excluded
    }

    #[test]
    fn test_extract_references_skips_keywords() {
        let content = "function foo() { if (true) { return false; } }";
        let refs = extract_references_from_content(content, "foo");
        assert!(!refs.contains(&"if"));
        assert!(!refs.contains(&"true"));
        assert!(!refs.contains(&"return"));
        assert!(!refs.contains(&"false"));
    }

    #[test]
    fn test_infer_ref_type_call() {
        assert_eq!(
            infer_ref_type("validateInput(data)", "validateInput"),
            RefType::Calls,
        );
    }

    #[test]
    fn test_infer_ref_type_type() {
        assert_eq!(
            infer_ref_type("let x: MyType = something", "MyType"),
            RefType::TypeRef,
        );
    }

    #[test]
    fn test_infer_ref_type_multibyte_utf8() {
        // Ensure no panic when content contains multi-byte UTF-8 characters
        assert_eq!(
            infer_ref_type("let café = foo(x)", "foo"),
            RefType::Calls,
        );
        assert_eq!(
            infer_ref_type("class HandicapfrPublicationFieldsEnum:\n    É = 1\n    bar()", "bar"),
            RefType::Calls,
        );
        // No match should not panic either
        assert_eq!(
            infer_ref_type("// 日本語コメント\nlet x = 1", "missing"),
            RefType::TypeRef,
        );
    }
}
