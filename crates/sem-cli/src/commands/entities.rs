use std::path::Path;

use colored::Colorize;
use sem_core::parser::plugins::create_default_registry;

pub struct EntitiesOptions {
    pub cwd: String,
    pub file_path: String,
    pub json: bool,
}

pub fn entities_command(opts: EntitiesOptions) {
    let root = Path::new(&opts.cwd);
    let registry = create_default_registry();

    let full_path = root.join(&opts.file_path);
    let content = match std::fs::read_to_string(&full_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{} Cannot read '{}': {}", "error:".red().bold(), opts.file_path, e);
            std::process::exit(1);
        }
    };

    let plugin = match registry.get_plugin(&opts.file_path) {
        Some(p) => p,
        None => {
            eprintln!("{} No parser for '{}'", "error:".red().bold(), opts.file_path);
            std::process::exit(1);
        }
    };

    let entities = plugin.extract_entities(&content, &opts.file_path);

    if opts.json {
        let output: Vec<_> = entities.iter().map(|e| {
            serde_json::json!({
                "name": e.name,
                "type": e.entity_type,
                "start_line": e.start_line,
                "end_line": e.end_line,
                "parent_id": e.parent_id,
            })
        }).collect();
        println!("{}", serde_json::to_string(&output).unwrap());
    } else {
        println!("{} {}\n", "entities:".green().bold(), opts.file_path.bold());

        // Build parent lookup for indentation
        let parent_ids: std::collections::HashSet<&str> = entities
            .iter()
            .filter_map(|e| e.parent_id.as_deref())
            .collect();

        for entity in &entities {
            let indent = if entity.parent_id.is_some() { "    " } else { "  " };
            let is_parent = parent_ids.contains(entity.id.as_str())
                || entities.iter().any(|e| e.parent_id.as_deref() == Some(&entity.id));

            let name_display = if is_parent {
                entity.name.bold().to_string()
            } else {
                entity.name.bold().to_string()
            };

            println!(
                "{}{} {} (L{}:{})",
                indent,
                entity.entity_type.dimmed(),
                name_display,
                entity.start_line,
                entity.end_line,
            );
        }
    }
}
