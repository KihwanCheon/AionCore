//! Integration tests for resolving current Git branches in one batch.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use aionui_api_types::WebSocketMessage;
use aionui_file::{FileError, FileService, IFileService};
use aionui_realtime::EventBroadcaster;

struct NoopBroadcaster;

impl EventBroadcaster for NoopBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

fn make_service(root: &Path) -> FileService {
    FileService::new(Arc::new(NoopBroadcaster), vec![root.to_path_buf()])
}

#[tokio::test]
async fn deduplicates_workspaces_and_isolates_non_repositories() {
    let root = tempfile::tempdir().unwrap();
    let repository_path = root.path().join("repository");
    let non_repository_path = root.path().join("plain");
    fs::create_dir(&repository_path).unwrap();
    fs::create_dir(&non_repository_path).unwrap();
    let mut options = git2::RepositoryInitOptions::new();
    options.initial_head("feature/batch-branches");
    git2::Repository::init_opts(&repository_path, &options).unwrap();
    let repository_path = repository_path.to_string_lossy().into_owned();
    let non_repository_path = non_repository_path.to_string_lossy().into_owned();
    let service = make_service(root.path());

    let result = service
        .get_git_branches(&[repository_path.clone(), non_repository_path.clone(), repository_path])
        .await
        .unwrap();

    assert_eq!(result.len(), 2);
    assert_eq!(result[0].branch.as_deref(), Some("feature/batch-branches"));
    assert_eq!(result[1].branch, None);
}

#[tokio::test]
async fn returns_short_commit_for_detached_head() {
    let root = tempfile::tempdir().unwrap();
    let repository = git2::Repository::init(root.path()).unwrap();
    fs::write(root.path().join("readme.md"), "test").unwrap();
    let mut index = repository.index().unwrap();
    index.add_path(Path::new("readme.md")).unwrap();
    let tree_id = index.write_tree().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    let signature = git2::Signature::now("test", "test@example.com").unwrap();
    let commit_id = repository
        .commit(Some("HEAD"), &signature, &signature, "initial", &tree, &[])
        .unwrap();
    repository.set_head_detached(commit_id).unwrap();
    let service = make_service(root.path());

    let result = service
        .get_git_branches(&[root.path().to_string_lossy().into_owned()])
        .await
        .unwrap();

    assert_eq!(result[0].branch, Some(commit_id.to_string().chars().take(7).collect()));
}

#[tokio::test]
async fn rejects_invalid_batch_input() {
    let root = tempfile::tempdir().unwrap();
    let service = make_service(root.path());
    let too_many = vec!["workspace".to_owned(); 257];

    assert!(matches!(
        service.get_git_branches(&too_many).await,
        Err(FileError::BadRequest(_))
    ));
    assert!(matches!(
        service.get_git_branches(&[String::new()]).await,
        Err(FileError::BadRequest(_))
    ));
}
