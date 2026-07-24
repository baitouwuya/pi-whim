use std::{
    collections::BTreeMap,
    env, fs,
    path::{Path, PathBuf},
};

use serde_json::Value;
use walkdir::WalkDir;

#[derive(Clone)]
struct Record {
    provider: String,
    id: String,
    name: String,
    reasoning: bool,
    supports_images: bool,
    thinking_level_map: Vec<(String, Option<String>)>,
}

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let catalog_root = manifest.join("../../vendor/pi-mono/packages/ai/src/providers/data");
    println!("cargo:rerun-if-changed={}", catalog_root.display());

    let mut records = BTreeMap::<(String, String), Record>::new();
    if catalog_root.is_dir() {
        for entry in WalkDir::new(&catalog_root)
            .max_depth(1)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry.path().extension().and_then(|value| value.to_str()) == Some("json")
            })
        {
            read_catalog_file(entry.path(), &mut records);
        }
    }

    let output = render_catalog(records.into_values());
    let output_path = PathBuf::from(env::var("OUT_DIR").expect("build output directory"))
        .join("bundled_model_capabilities.rs");
    fs::write(output_path, output).expect("write bundled model capability index");
}

fn read_catalog_file(path: &Path, records: &mut BTreeMap<(String, String), Record>) {
    let Ok(contents) = fs::read_to_string(path) else {
        return;
    };
    let Ok(Value::Object(models)) = serde_json::from_str::<Value>(&contents) else {
        return;
    };
    for value in models.values() {
        let Some(id) = value.get("id").and_then(Value::as_str) else {
            continue;
        };
        let Some(provider) = value.get("provider").and_then(Value::as_str) else {
            continue;
        };
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(id)
            .to_owned();
        let reasoning = value
            .get("reasoning")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let supports_images = value
            .get("input")
            .and_then(Value::as_array)
            .is_some_and(|inputs| inputs.iter().any(|input| input.as_str() == Some("image")));
        let mut thinking_level_map = value
            .get("thinkingLevelMap")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .map(|(level, mapped)| (level.clone(), mapped.as_str().map(str::to_owned)))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        thinking_level_map.sort_by(|left, right| left.0.cmp(&right.0));
        let record = Record {
            provider: provider.to_owned(),
            id: id.to_owned(),
            name,
            reasoning,
            supports_images,
            thinking_level_map,
        };
        records.insert((provider.to_owned(), id.to_owned()), record);
    }
}

fn render_catalog(records: impl Iterator<Item = Record>) -> String {
    let mut output = String::from(
        "// Generated from vendor/pi-mono provider data. Do not edit.\n\
         pub(super) static BUNDLED_CAPABILITIES: &[BundledCapability] = &[\n",
    );
    for record in records {
        let map = record
            .thinking_level_map
            .iter()
            .map(|(level, value)| match value {
                Some(value) => format!("({level:?}, Some({value:?}))"),
                None => format!("({level:?}, None)"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        output.push_str(&format!(
            "    BundledCapability {{ provider: {:?}, id: {:?}, name: {:?}, reasoning: {}, supports_images: {}, thinking_level_map: &[{}] }},\n",
            record.provider,
            record.id,
            record.name,
            record.reasoning,
            record.supports_images,
            map,
        ));
    }
    output.push_str("];\n");
    output
}
