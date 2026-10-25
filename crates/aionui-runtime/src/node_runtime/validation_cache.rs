use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use tokio::sync::Mutex;
use tokio::time::Instant;

use super::{NodeRuntimeError, ResolvedCommand, ResolvedNodeRuntime, ResolvedNodeSource};

const VALIDATION_TIMEOUT: Duration = Duration::from_secs(60);

/// Reuse successful version probes while entrypoints are unchanged. The bounded
/// age also rechecks dependencies that are not individual fingerprint inputs.
pub(super) struct RuntimeValidationCache {
    entry: Mutex<Option<ValidatedRuntime>>,
    max_age: Duration,
}

struct ValidatedRuntime {
    runtime: ResolvedNodeRuntime,
    fingerprint: RuntimeFingerprint,
    validated_at: Instant,
}

#[derive(PartialEq, Eq)]
struct RuntimeFingerprint {
    root: PathBuf,
    source: ResolvedNodeSource,
    npm: ResolvedCommand,
    npx: ResolvedCommand,
    files: Vec<(PathBuf, Option<FileFingerprint>)>,
}

#[derive(PartialEq, Eq)]
struct FileFingerprint {
    len: u64,
    modified: SystemTime,
    permissions: fs::Permissions,
}

impl RuntimeValidationCache {
    pub(super) const fn new(max_age: Duration) -> Self {
        Self {
            entry: Mutex::const_new(None),
            max_age,
        }
    }

    pub(super) async fn resolve<F, Fut>(
        &self,
        candidate: &ResolvedNodeRuntime,
        validate: F,
    ) -> Result<ResolvedNodeRuntime, NodeRuntimeError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<ResolvedNodeRuntime, NodeRuntimeError>>,
    {
        // Keep validation single-flight, including when several callers observe
        // the same expired or changed runtime at once.
        let mut entry = self.entry.lock().await;
        if let Some(cached) = entry.as_ref()
            && cached.validated_at.elapsed() < self.max_age
            && fingerprint(candidate).is_some_and(|current| current == cached.fingerprint)
        {
            return Ok(cached.runtime.clone());
        }

        *entry = None;
        // A stalled child must not leave every runtime user waiting on this
        // shared validation forever. Dropping the probe kills its child process.
        let runtime = tokio::time::timeout(VALIDATION_TIMEOUT, validate())
            .await
            .map_err(|_| NodeRuntimeError::managed_invalid("managed Node runtime validation timed out after 60s"))??;
        *entry = validated_entry(&runtime);
        Ok(runtime)
    }

    pub(super) async fn remember(&self, runtime: &ResolvedNodeRuntime) {
        *self.entry.lock().await = validated_entry(runtime);
    }
}

fn validated_entry(runtime: &ResolvedNodeRuntime) -> Option<ValidatedRuntime> {
    Some(ValidatedRuntime {
        runtime: runtime.clone(),
        fingerprint: fingerprint(runtime)?,
        validated_at: Instant::now(),
    })
}

fn fingerprint(runtime: &ResolvedNodeRuntime) -> Option<RuntimeFingerprint> {
    let mut required = vec![
        runtime.node_path.clone(),
        runtime.npm_path.clone(),
        runtime.npx_path.clone(),
    ];
    required.extend(
        runtime
            .npm_args_prefix
            .iter()
            .chain(&runtime.npx_args_prefix)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute()),
    );
    required.sort();
    required.dedup();
    let mut files = Vec::with_capacity(required.len() + 4);
    for path in required {
        files.push((path.clone(), Some(file_fingerprint(&path).ok()?)));
    }
    let optional = [
        runtime.root.join("node_modules/npm/package.json"),
        runtime.root.join("lib/node_modules/npm/package.json"),
    ]
    .into_iter()
    .chain(
        runtime
            .env
            .iter()
            .filter(|(key, _)| key == "npm_config_userconfig" || key == "npm_config_globalconfig")
            .map(|(_, value)| PathBuf::from(value)),
    );
    for path in optional {
        let stamp = match file_fingerprint(&path) {
            Ok(stamp) => Some(stamp),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
        };
        files.push((path, stamp));
    }
    Some(RuntimeFingerprint {
        root: runtime.root.clone(),
        source: runtime.source,
        npm: runtime.npm_command(),
        npx: runtime.npx_command(),
        files,
    })
}

fn file_fingerprint(path: &Path) -> std::io::Result<FileFingerprint> {
    let metadata = fs::metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::other("runtime entrypoint is not a file"));
    }
    Ok(FileFingerprint {
        len: metadata.len(),
        modified: metadata.modified()?,
        permissions: metadata.permissions(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn runtime(root: &Path) -> ResolvedNodeRuntime {
        let node = root.join("node");
        let script = root.join("npm-cli.js");
        fs::write(&node, "node fixture").unwrap();
        fs::write(&script, "npm fixture").unwrap();
        ResolvedNodeRuntime {
            source: ResolvedNodeSource::Managed,
            root: root.to_owned(),
            version: semver::Version::new(24, 11, 0),
            node_path: node.clone(),
            npm_path: node.clone(),
            npm_args_prefix: vec![script.clone().into_os_string()],
            npx_path: node,
            npx_args_prefix: vec![script.into_os_string()],
            env: vec![],
        }
    }

    #[tokio::test]
    async fn concurrent_unchanged_requests_share_successful_validation() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        let probes = AtomicUsize::new(0);
        let resolve = || {
            cache.resolve(&runtime, || async {
                probes.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(runtime.clone())
            })
        };
        let (first, second, third) = tokio::join!(resolve(), resolve(), resolve());
        assert!(first.is_ok() && second.is_ok() && third.is_ok());
        assert_eq!(probes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn changed_entrypoint_is_revalidated_and_failed_validation_is_not_cached() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&runtime).await;
        fs::write(&runtime.npm_args_prefix[0], "changed npm fixture").unwrap();
        let error = cache
            .resolve(&runtime, || async {
                Err(NodeRuntimeError::managed_invalid("broken npm"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("broken npm"));
        assert!(cache.entry.lock().await.is_none());
        assert!(cache.resolve(&runtime, || async { Ok(runtime.clone()) }).await.is_ok());
    }

    #[tokio::test]
    async fn deleted_executable_invalidates_even_a_recent_validation() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&runtime).await;
        fs::remove_file(&runtime.node_path).unwrap();
        let error = cache
            .resolve(&runtime, || async {
                Err(NodeRuntimeError::managed_invalid("missing node"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("missing node"));
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_revalidates_unchanged_files_and_returns_the_new_version() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&runtime).await;
        tokio::time::advance(Duration::from_secs(300)).await;
        let mut refreshed = runtime.clone();
        refreshed.version = semver::Version::new(24, 12, 0);
        let resolved = cache.resolve(&runtime, || async { Ok(refreshed) }).await.unwrap();
        assert_eq!(resolved.version, semver::Version::new(24, 12, 0));
    }

    #[tokio::test(start_paused = true)]
    async fn expired_validation_cannot_mask_a_failure_with_unchanged_entrypoints() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&runtime).await;
        tokio::time::advance(Duration::from_secs(300)).await;
        let error = cache
            .resolve(&runtime, || async {
                Err(NodeRuntimeError::managed_invalid("dependency failure"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("dependency failure"));
        assert!(cache.entry.lock().await.is_none());
    }

    #[tokio::test]
    async fn newly_created_npm_configuration_is_revalidated() {
        let root = tempfile::tempdir().unwrap();
        let mut runtime = runtime(root.path());
        let config = root.path().join("npmrc");
        runtime
            .env
            .push(("npm_config_userconfig".into(), config.clone().into_os_string()));
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&runtime).await;
        fs::write(config, "prefix=changed").unwrap();
        let error = cache
            .resolve(&runtime, || async {
                Err(NodeRuntimeError::managed_invalid("config changed"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("config changed"));
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_validation_times_out_and_releases_waiters_for_a_retry() {
        let root = tempfile::tempdir().unwrap();
        let runtime = runtime(root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        let error = cache.resolve(&runtime, std::future::pending).await.unwrap_err();
        assert!(error.to_string().contains("timed out after 60s"));
        assert!(cache.resolve(&runtime, || async { Ok(runtime.clone()) }).await.is_ok());
    }

    #[tokio::test]
    async fn installing_a_different_root_or_changing_configuration_requires_validation() {
        let first_root = tempfile::tempdir().unwrap();
        let second_root = tempfile::tempdir().unwrap();
        let first = runtime(first_root.path());
        let mut second = runtime(second_root.path());
        let cache = RuntimeValidationCache::new(Duration::from_secs(300));
        cache.remember(&first).await;
        assert!(cache.resolve(&second, || async { Ok(second.clone()) }).await.is_ok());
        second.env.push(("npm_config_prefix".into(), "changed-prefix".into()));
        let error = cache
            .resolve(&second, || async {
                Err(NodeRuntimeError::managed_invalid("changed configuration"))
            })
            .await
            .unwrap_err();
        assert!(error.to_string().contains("changed configuration"));
    }
}
