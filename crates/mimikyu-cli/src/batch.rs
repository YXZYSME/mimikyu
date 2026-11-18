use std::fs;
use std::path::Path;

use mimikyu_core::daemon::{normalize_batch_ops_value, parse_batch_actions};
use mimikyu_core::error::{MimikyuError, Result};
use serde_json::Value;

use crate::cli::BatchArgs;

pub fn load_normalized_batch_actions(args: &BatchArgs) -> Result<Vec<Value>> {
    let raw_str = match (&args.file, &args.ops) {
        (Some(_), Some(_)) => {
            return Err(MimikyuError::InvalidConfig(
                "specify either --file or --ops, not both".to_string(),
            ));
        }
        (None, None) => {
            return Err(MimikyuError::InvalidConfig(
                "batch requires either --file <path> or --ops <json>".to_string(),
            ));
        }
        (Some(path), None) => read_batch_file(path)?,
        (None, Some(ops)) => ops.clone(),
    };

    let raw_val: Value = serde_json::from_str(&raw_str).map_err(|e| {
        MimikyuError::InvalidConfig(format!("invalid batch JSON: {e}"))
    })?;

    // Validate full deserializability into Vec<BatchAction> up front on CLI side too
    let normalized = normalize_batch_ops_value(raw_val.clone())?;
    let _ = parse_batch_actions(raw_val)?;
    Ok(normalized)
}

fn read_batch_file(path: &Path) -> Result<String> {
    if path.as_os_str().is_empty() || path.is_dir() || !path.exists() {
        return Err(MimikyuError::InvalidConfig(format!(
            "batch file '{}' does not exist or is not a file",
            path.display()
        )));
    }
    Ok(fs::read_to_string(path)?)
}
