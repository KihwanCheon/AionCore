//! Short-lived desktop database maintenance; never boots the application server.

use aionui_db::fork_extensions::database_maintenance::{compact, inspect};
use std::{path::Path, process::ExitCode};

pub(crate) async fn run_database(data_dir: &Path, compact_requested: bool) -> ExitCode {
    let path = data_dir.join("aionui-backend.db");
    let result = if compact_requested {
        compact(&path)
            .await
            .map(|value| serde_json::json!({"success": true, "data": value}))
    } else {
        inspect(&path)
            .await
            .map(|value| serde_json::json!({"success": true, "data": value}))
    };
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            println!("{}", serde_json::json!({"success": false, "code": error.to_string()}));
            eprintln!("Database maintenance failed: {error:?}");
            ExitCode::FAILURE
        }
    }
}
