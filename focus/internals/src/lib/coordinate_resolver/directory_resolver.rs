use std::{
    iter::FromIterator,
    path::{Path, PathBuf},
};

use super::*;

/// Resolves directories verbatim
pub struct DirectoryResolver {
    #[allow(dead_code)]
    cache_root: PathBuf,
}

impl Resolver for DirectoryResolver {
    fn new(cache_root: &Path) -> Self {
        Self {
            cache_root: cache_root.join("directory"),
        }
    }

    fn resolve(
        &self,
        request: &ResolutionRequest,
        _cache_options: &CacheOptions,
        _app: Arc<App>,
    ) -> Result<ResolutionResult> {
        let paths =
            BTreeSet::<PathBuf>::from_iter(request.coordinate_set.underlying().iter().filter_map(
                |target| match target {
                    Target::Directory(inner) => Some(PathBuf::from(inner)),
                    _ => unreachable!(),
                },
            ));
        let package_infos: BTreeMap<_, _> = request
            .coordinate_set
            .underlying()
            .iter()
            .map(|target| match &target {
                Target::Directory(directory) => (
                    DependencyKey::Path(directory.into()),
                    DependencyValue::Path {
                        path: directory.into(),
                    },
                ),
                _ => unreachable!(
                    "Bad target type (expected directory): {:?}",
                    &target
                ),
            })
            .collect();

        Ok(ResolutionResult {
            paths,
            package_deps: package_infos,
        })
    }
}
