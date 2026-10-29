use std::{env, fs, path::PathBuf};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("../integrations/data_connectors");
    println!("cargo:rerun-if-changed={}", root.display());
    let mut integrations = Vec::new();
    for entry in fs::read_dir(&root).expect("Cannot read integrations directory") {
        let path = entry.expect("Cannot read integration entry").path();
        if !path.is_dir() || !path.join("mod.rs").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .unwrap()
            .to_str()
            .expect("Integration name must be UTF-8");
        assert!(
            name.chars().enumerate().all(|(position, character)| {
                character == '_'
                    || character.is_ascii_alphabetic()
                    || (position > 0 && character.is_ascii_digit())
            }),
            "Integration folder must be a Rust identifier: {name}"
        );
        let investigation = path.join("investigation.rs").is_file();
        integrations.push((
            name.to_owned(),
            path.join("mod.rs").canonicalize().unwrap(),
            investigation,
        ));
    }
    integrations.sort_by(|left, right| left.0.cmp(&right.0));
    let mut generated = String::new();
    for (name, path, _) in &integrations {
        generated.push_str(&format!(
            "#[path = {:?}]\npub mod {name};\n",
            path.to_str().unwrap()
        ));
    }
    // AI support and translatable query languages are derived from the dialect file, so neither
    // can drift from the translator.
    generated.push_str("pub fn registered() -> Vec<IntegrationDescriptor> { vec![\n");
    for (name, _, investigation) in &integrations {
        let languages = if *investigation {
            format!(
                "crate::investigation_dialect::input_languages({name}::investigation::dialect())"
            )
        } else {
            "Vec::new()".to_owned()
        };
        generated.push_str(&format!(
            "IntegrationDescriptor {{ ai_investigation: {investigation}, query_languages: {languages}, ..{name}::descriptor() }},\n"
        ));
    }
    generated.push_str("] }\n");
    generated.push_str("pub fn investigation_dialects() -> Vec<&'static dyn crate::investigation_dialect::InvestigationDialect> { vec![\n");
    for (name, _, investigation) in &integrations {
        if *investigation {
            generated.push_str(&format!("{name}::investigation::dialect(),\n"));
        }
    }
    generated.push_str("] }\n");
    generated.push_str("pub fn investigation_dialect(id: &str) -> Option<&'static dyn crate::investigation_dialect::InvestigationDialect> { match id {\n");
    for (name, _, investigation) in &integrations {
        if *investigation {
            generated.push_str(&format!(
                "{name:?} => Some({name}::investigation::dialect()),\n"
            ));
        }
    }
    generated.push_str("_ => None, } }\n");
    generated.push_str("pub fn validate_registry() { let descriptors = registered(); let mut ids = std::collections::HashSet::new(); for descriptor in &descriptors { assert!(ids.insert(descriptor.id), \"Duplicate integration id\"); }\n");
    for (name, _, _) in &integrations {
        generated.push_str(&format!("assert_eq!({name}::descriptor().id, {name:?}, \"Integration id must match folder\");\n"));
    }
    generated.push_str("}\npub fn connect(id: &str, config: &ConnectionConfig) -> Result<std::sync::Arc<dyn Connector>, ConnectorError> { validate_registry(); match id {\n");
    for (name, _, _) in &integrations {
        generated.push_str(&format!(
            "{name:?} => {name}::connect(config).map(|inner| logged_connector({name:?}, inner)),\n"
        ));
    }
    generated.push_str("_ => Err(ConnectorError::invalid_connection()), } }\n");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("data_connectors.rs");
    fs::write(output, generated).expect("Cannot write integration registry");
}
