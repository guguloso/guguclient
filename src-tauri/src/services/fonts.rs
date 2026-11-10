//! Per-instance custom fonts (`fonts:*`). Port of `FontsService`: the chosen
//! font is copied to `<instance>/font/CustomFont.ttf` and applied by the
//! modded runtime at launch.

use std::path::PathBuf;

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::ipc::Router;
use crate::services::instances::{self, product_of};
use crate::services::roblox_layout::instance_config_dir;
use crate::state::AppState;

const FONT_FILE_EXTENSIONS: [&str; 3] = ["ttf", "otf", "ttc"];
const FONT_MAGIC_SIGNATURES: [[u8; 4]; 5] = [[0, 1, 0, 0], *b"OTTO", *b"ttcf", *b"true", *b"typ1"];
const INVALID_FONT_MESSAGE: &str = "That file does not look like a valid font. Choose a .ttf, .otf, or .ttc file.";

/// `getInstanceCustomFontPath`.
pub fn custom_font_path(state: &AppState, id: &str, product: &str) -> Option<PathBuf> {
    instance_config_dir(&state.paths.base, id, product).ok().map(|d| d.join("font").join("CustomFont.ttf"))
}

fn is_valid_font(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && FONT_MAGIC_SIGNATURES.iter().any(|sig| bytes.starts_with(sig))
}

fn failure(message: &str) -> Value {
    json!({ "ok": false, "message": message })
}

async fn set_font(state: &AppState, id: &str) -> Value {
    let Some(instance) = state.instances.get(&state.paths.base, id).filter(|i| product_of(i) == "player") else {
        return failure("Custom fonts are only supported for Player instances.");
    };
    let picked = rfd::AsyncFileDialog::new()
        .set_title("Choose Font")
        .add_filter("Fonts", &FONT_FILE_EXTENSIONS)
        .add_filter("All files", &["*"])
        .pick_file()
        .await;
    let Some(picked) = picked else { return Value::Null };
    let source = picked.path().to_path_buf();
    let extension = source.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if !FONT_FILE_EXTENSIONS.contains(&extension.as_str()) {
        return failure(INVALID_FONT_MESSAGE);
    }
    let bytes = match std::fs::read(&source) {
        Ok(b) => b,
        Err(error) => {
            tracing::error!("Failed to read chosen font file: {error}");
            return failure("Failed to read the selected file.");
        }
    };
    if !is_valid_font(&bytes) {
        return failure(INVALID_FONT_MESSAGE);
    }
    let font = json!({
        "fileName": source.file_name().and_then(|n| n.to_str()).unwrap_or(""),
        "assetHash": format!("{:x}", Sha256::digest(&bytes)),
    });
    let Some(destination) = custom_font_path(state, id, product_of(&instance)) else {
        return failure("Failed to save the selected font.");
    };
    let saved = destination.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|_| std::fs::write(&destination, &bytes));
    if let Err(error) = saved {
        tracing::error!("Failed to store custom font file: {error}");
        return failure("Failed to save the selected font.");
    }
    let mut updates = Map::new();
    updates.insert("customFont".into(), font.clone());
    instances::update(state, id, updates);
    json!({ "ok": true, "font": font })
}

fn clear_font(state: &AppState, id: &str) -> bool {
    let Some(instance) = state.instances.get(&state.paths.base, id) else { return false };
    if let Some(path) = custom_font_path(state, id, product_of(&instance)).filter(|p| p.exists()) {
        let _ = std::fs::remove_file(path);
    }
    let mut updates = Map::new();
    updates.insert("customFont".into(), Value::Null);
    instances::update(state, id, updates);
    true
}

pub fn register(router: &mut Router) {
    router
        .handle("fonts:setFont", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(set_font(&ctx.state, &id).await)
        })
        .handle("fonts:clearFont", |ctx, args| async move {
            let id: String = args.get(0)?;
            Ok(clear_font(&ctx.state, &id))
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn font_signatures() {
        assert!(super::is_valid_font(b"OTTO\0\0"));
        assert!(super::is_valid_font(&[0, 1, 0, 0, 9]));
        assert!(!super::is_valid_font(b"PK\x03\x04"));
    }
}
