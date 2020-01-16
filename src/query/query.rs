use std::collections::VecDeque;
use std::path::Path;

use async_std::fs::read_dir;
use async_std::path::PathBuf;
use futures::executor::block_on;
use futures::join;
use futures::StreamExt;

use crate::configurator::Configs;
use crate::query::checkers::{BundleChecker, Checker, HiddenChecker, IgnoreChecker, SymlinkChecker};
use crate::query::matcher;
use crate::query::service::Service;
use crate::utils::cache::CacheManager;
use crate::utils::serde::serializer;

pub struct QueryProcessor {
    config: Configs,
    condition_checker: Box<dyn Checker>,
    terminate_checkers: Vec<Box<dyn Checker>>,
}

impl QueryProcessor {
    const CONFIG_PATH: &'static str = "settings.yaml";

    /// New query processor
    pub fn new() -> Self {
        let config = Configs::from(Path::new(Self::CONFIG_PATH))
            .expect("settings.yaml is missing");
        let ignore_paths = config.get_ignore_paths();
        QueryProcessor {
            config,
            condition_checker: Box::new(BundleChecker {}),
            terminate_checkers: if ignore_paths.is_empty() {
                vec![Box::new(HiddenChecker {})]
            } else {
                vec![
                    Box::new(HiddenChecker {}),
                    Box::new(IgnoreChecker::new(ignore_paths)),
                    Box::new(SymlinkChecker {})
                ]
            },
        }
    }

    /// Query based on the request, and return serialized bytes of the services
    pub fn query(&self, req: String) -> Vec<u8> {
        block_on(self.async_query(req))
    }

    /// Async query
    async fn async_query(&self, req: String) -> Vec<u8> {
        let (cached_services, updated_services) = join!(
            self.query_cached_services(&req),
            self.query_updated_services(&req)
        );
        cached_services.into_iter()
            .chain(updated_services.into_iter())
            .collect()
    }

    /// Cached services are either loaded from cache or generated by walking through directories
    async fn query_cached_services(&self, req: &str) -> Vec<u8> {
        let mut cache_manager = CacheManager::new().await;
        match Some(cache_manager.bunch_read().await) {
            Some(cache) if !cache.is_empty() => cache,
            _ => {
                let mut res: Vec<Service> = vec![];
                for path in self.config.get_internal_cached() {
                    let paths = self.recursively_iterate(path).await
                        .into_iter()
                        .map(Service::new)
                        .collect::<Vec<_>>();
                    res.extend(paths);
                }
                cache_manager.bunch_save(res).await
            }
        }.into_iter()
            .filter(|service| service.path.to_str().is_some())
            .filter(|service| matcher::match_query(&req, service.path.to_str().unwrap()))
            .flat_map(serializer::serialize_to_bytes)
            .collect()
    }

    async fn query_updated_services(&self, req: &str) -> Vec<u8> {
        let mut res = vec![];
        for path in self.config.get_internal_updated() {
            let bytes = self.recursively_iterate(path).await
                .into_iter()
                .filter(|path| path.to_str().is_some())
                .filter(|path| matcher::match_query(&req, path.to_str().unwrap()))
                .map(Service::new)
                .flat_map(serializer::serialize_to_bytes)
                .collect::<Vec<_>>();
            res.extend(bytes)
        }
        res
    }

    /// Recursively iterate through files and folders, and return all legit file paths
    async fn recursively_iterate(&self, entry: PathBuf) -> Vec<PathBuf> {
        if self.terminate_checkers.iter()
            .any(|checker| checker.is_legit(&entry)) {
            vec![]
        } else if self.condition_checker.is_legit(&entry) {
            vec![entry]
        } else {
            let (mut res, mut remaining) = self.separate_files_and_dirs(entry).await;
            while let Some(entry) = remaining.pop_front() {
                let (processed, unprocessed) = self.separate_files_and_dirs(entry).await;
                res.extend(processed);
                remaining.extend(unprocessed);
            }
            res
        }
    }

    /// walk through all files in the given entry, and return paths for files and directories
    async fn separate_files_and_dirs(&self, entry: PathBuf) -> (Vec<PathBuf>, VecDeque<PathBuf>) {
        let mut processed = Vec::new();
        let mut folders = VecDeque::new();
        let mut read_folder = read_dir(&entry).await.expect("Unwrap folder");
        while let Some(Ok(path)) = read_folder.next().await {
            let path = path.path();
            if self.terminate_checkers.iter().any(|checker| checker.is_legit(&path)) {
                continue;
            } else if self.condition_checker.is_legit(&path) {
                processed.push(path)
            } else {
                folders.push_back(path)
            }
        }
        (processed, folders)
    }
}

#[cfg(test)]
mod query_test {
    use async_std::path::PathBuf;
    use futures::executor::block_on;

    use crate::query::query::QueryProcessor;

    type QP = QueryProcessor;

    const APP_PATH: &str = "/System/Applications/Books.app";
    const APP_FOLDER_PATH: &str = "/System/Applications";

    #[test]
    fn test_walk_dir_single() {
        let processor = QP::new();
        let single_file = PathBuf::from(APP_PATH);
        let expected = PathBuf::from(APP_PATH);
        let res = block_on(processor.recursively_iterate(single_file));
        assert_eq!(&expected, &res[0]);
    }

    #[test]
    fn test_walk_dir_inside_book() {
        let processor = QP::new();
        let content = PathBuf::from(APP_FOLDER_PATH);
        let res = block_on(processor.recursively_iterate(content));
        assert_eq!(52, res.len());
    }
}
