#![doc = include_str!("../README.md")]

pub mod artifacts;

pub use artifacts::{CompilerInput, CompilerOutput, EvmVersion};
use std::collections::btree_map::Entry;

pub mod cache;

mod compile;
pub use compile::*;

mod config;
pub use config::{AllowedLibPaths, ArtifactOutput, ProjectPathsConfig, SolcConfig};

pub mod remappings;

use crate::{artifacts::Source, cache::SolFilesCache};

pub mod error;
pub mod utils;
use crate::artifacts::Sources;
use error::Result;
use std::{
    collections::{BTreeMap, HashMap},
    convert::TryInto,
    fmt, fs, io,
    path::PathBuf,
};

/// Handles contract compiling
#[derive(Debug)]
pub struct Project {
    /// The layout of the
    pub paths: ProjectPathsConfig,
    /// Where to find solc
    pub solc: Solc,
    /// How solc invocation should be configured.
    pub solc_config: SolcConfig,
    /// Whether caching is enabled
    pub cached: bool,
    /// How to handle compiler output
    pub artifacts: ArtifactOutput,
    /// Errors/Warnings which match these error codes are not going to be logged
    pub ignored_error_codes: Vec<u64>,
    /// The paths which will be allowed for library inclusion
    pub allowed_lib_paths: AllowedLibPaths,
}

impl Project {
    /// Configure the current project
    ///
    /// # Example
    ///
    /// ```rust
    /// use ethers_solc::Project;
    /// let config = Project::builder().build().unwrap();
    /// ```
    pub fn builder() -> ProjectBuilder {
        ProjectBuilder::default()
    }

    fn write_cache_file(&self, sources: Sources) -> Result<()> {
        let cache = SolFilesCache::builder()
            .root(&self.paths.root)
            .solc_config(self.solc_config.clone())
            .insert_files(sources)?;
        if let Some(cache_dir) = self.paths.cache.parent() {
            fs::create_dir_all(cache_dir)?
        }
        cache.write(&self.paths.cache)
    }

    /// Returns all sources found under the project's sources path
    pub fn sources(&self) -> io::Result<Sources> {
        Source::read_all_from(self.paths.sources.as_path())
    }

    /// Attempts to read all unique libraries that are used as imports like "hardhat/console.sol"
    fn resolved_libraries(
        &self,
        sources: &Sources,
    ) -> io::Result<BTreeMap<PathBuf, (Source, PathBuf)>> {
        let mut libs = BTreeMap::default();
        for source in sources.values() {
            for import in source.parse_imports() {
                if let Some(lib) = utils::resolve_library(&self.paths.libraries, import) {
                    if let Entry::Vacant(entry) = libs.entry(import.into()) {
                        entry.insert((Source::read(&lib)?, lib));
                    }
                }
            }
        }
        Ok(libs)
    }

    /// Attempts to compile the contracts found at the configured location.
    ///
    /// NOTE: this does not check if the contracts were successfully compiled, see
    /// `CompilerOutput::has_error` instead.

    /// NB: If the `svm` feature is enabled, this function will automatically detect
    /// solc versions across files.
    pub fn compile(&self) -> Result<ProjectCompileOutput> {
        let sources = self.sources()?;

        #[cfg(not(all(feature = "svm", feature = "async")))]
        {
            self.compile_with_version(&self.solc, sources)
        }
        #[cfg(all(feature = "svm", feature = "async"))]
        self.svm_compile(sources)
    }

    #[cfg(all(feature = "svm", feature = "async"))]
    fn svm_compile(&self, sources: Sources) -> Result<ProjectCompileOutput> {
        // split them by version
        let mut sources_by_version = BTreeMap::new();
        for (path, source) in sources.into_iter() {
            // will detect and install the solc version
            let version = Solc::detect_version(&source)?;
            // gets the solc binary for that version, it is expected tha this will succeed
            // AND find the solc since it was installed right above
            let mut solc = Solc::find_svm_installed_version(version.to_string())?
                .expect("solc should have been installed");

            if !self.allowed_lib_paths.0.is_empty() {
                solc = solc.arg("--allow-paths").arg(self.allowed_lib_paths.to_string());
            }
            let entry = sources_by_version.entry(solc).or_insert_with(BTreeMap::new);
            entry.insert(path, source);
        }

        // run the compilation step for each version
        let mut res = CompilerOutput::default();
        for (solc, sources) in sources_by_version {
            let output = self.compile_with_version(&solc, sources)?;
            if let ProjectCompileOutput::Compiled((compiled, _)) = output {
                res.errors.extend(compiled.errors);
                res.sources.extend(compiled.sources);
                res.contracts.extend(compiled.contracts);
            }
        }
        Ok(if res.contracts.is_empty() && res.errors.is_empty() {
            ProjectCompileOutput::Unchanged
        } else {
            ProjectCompileOutput::Compiled((res, &self.ignored_error_codes))
        })
    }

    pub fn compile_with_version(
        &self,
        solc: &Solc,
        mut sources: Sources,
    ) -> Result<ProjectCompileOutput> {
        // add all libraries to the source set while keeping track of their actual disk path
        let mut source_name_path = HashMap::new();
        let mut path_source_name = HashMap::new();
        for (import, (source, path)) in self.resolved_libraries(&sources)? {
            // inserting with absolute path here and keep track of the source name <-> path mappings
            sources.insert(path.clone(), source);
            path_source_name.insert(path.clone(), import.clone());
            source_name_path.insert(import, path);
        }

        // If there's a cache set, filter to only re-compile the files which were changed
        let sources = if self.cached && self.paths.cache.exists() {
            let cache = SolFilesCache::read(&self.paths.cache)?;
            let changed_files = cache.get_changed_files(sources, Some(&self.solc_config));
            if changed_files.is_empty() {
                return Ok(ProjectCompileOutput::Unchanged)
            }
            changed_files
        } else {
            sources
        };

        // replace absolute path with source name to make solc happy
        let sources = apply_mappings(sources, path_source_name);

        let input = CompilerInput::with_sources(sources)
            .normalize_evm_version(&solc.version()?)
            .with_remappings(self.paths.remappings.clone());
        let output = solc.compile(&input)?;
        if output.has_error() {
            return Ok(ProjectCompileOutput::Compiled((output, &self.ignored_error_codes)))
        }

        if self.cached {
            // reapply to disk paths
            let sources = apply_mappings(input.sources, source_name_path);
            // create cache file
            self.write_cache_file(sources)?;
        }

        self.artifacts.on_output(&output, &self.paths)?;
        Ok(ProjectCompileOutput::Compiled((output, &self.ignored_error_codes)))
    }
}

fn apply_mappings(sources: Sources, mut mappings: HashMap<PathBuf, PathBuf>) -> Sources {
    sources
        .into_iter()
        .map(|(import, source)| {
            if let Some(path) = mappings.remove(&import) {
                (path, source)
            } else {
                (import, source)
            }
        })
        .collect()
}

pub struct ProjectBuilder {
    /// The layout of the
    paths: Option<ProjectPathsConfig>,
    /// Where to find solc
    solc: Option<Solc>,
    /// How solc invocation should be configured.
    solc_config: Option<SolcConfig>,
    /// Whether caching is enabled, default is true.
    cached: bool,
    /// How to handle compiler output
    artifacts: Option<ArtifactOutput>,
    /// Which error codes to ignore
    pub ignored_error_codes: Vec<u64>,
    /// All allowed paths
    pub allowed_paths: Vec<PathBuf>,
}

impl ProjectBuilder {
    pub fn paths(mut self, paths: ProjectPathsConfig) -> Self {
        self.paths = Some(paths);
        self
    }

    pub fn solc(mut self, solc: impl Into<Solc>) -> Self {
        self.solc = Some(solc.into());
        self
    }

    pub fn solc_config(mut self, solc_config: SolcConfig) -> Self {
        self.solc_config = Some(solc_config);
        self
    }

    pub fn artifacts(mut self, artifacts: ArtifactOutput) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn ignore_error_code(mut self, code: u64) -> Self {
        self.ignored_error_codes.push(code);
        self
    }

    /// Disables cached builds
    pub fn ephemeral(mut self) -> Self {
        self.cached = false;
        self
    }

    /// Adds an allowed-path to the solc executable
    pub fn allowed_path<T: Into<PathBuf>>(mut self, path: T) -> Self {
        self.allowed_paths.push(path.into());
        self
    }

    /// Adds multiple allowed-path to the solc executable
    pub fn allowed_paths<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<PathBuf>,
    {
        for arg in args {
            self = self.allowed_path(arg);
        }
        self
    }

    pub fn build(self) -> Result<Project> {
        let Self {
            paths,
            solc,
            solc_config,
            cached,
            artifacts,
            ignored_error_codes,
            mut allowed_paths,
        } = self;

        let solc = solc.unwrap_or_default();
        let solc_config = solc_config.map(Ok).unwrap_or_else(|| {
            let version = solc.version()?;
            SolcConfig::builder().version(version.to_string()).build()
        })?;

        let paths = paths.map(Ok).unwrap_or_else(ProjectPathsConfig::current_hardhat)?;

        if allowed_paths.is_empty() {
            // allow every contract under root by default
            allowed_paths.push(paths.root.clone())
        }

        Ok(Project {
            paths,
            solc,
            solc_config,
            cached,
            artifacts: artifacts.unwrap_or_default(),
            ignored_error_codes,
            allowed_lib_paths: allowed_paths.try_into()?,
        })
    }
}

impl Default for ProjectBuilder {
    fn default() -> Self {
        Self {
            paths: None,
            solc: None,
            solc_config: None,
            cached: true,
            artifacts: None,
            ignored_error_codes: Vec::new(),
            allowed_paths: vec![],
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ProjectCompileOutput<'a> {
    /// Nothing to compile because unchanged sources
    Unchanged,
    Compiled((CompilerOutput, &'a [u64])),
}

impl<'a> fmt::Display for ProjectCompileOutput<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectCompileOutput::Unchanged => f.write_str("Nothing to compile"),
            ProjectCompileOutput::Compiled((output, ignored_error_codes)) => {
                output.diagnostics(ignored_error_codes).fmt(f)
            }
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    #[cfg(all(feature = "svm", feature = "async"))]
    fn test_build_all_versions() {
        use super::*;

        let paths = ProjectPathsConfig::builder()
            .root("./test-data/test-contract-versions")
            .sources("./test-data/test-contract-versions")
            .build()
            .unwrap();
        let project = Project::builder()
            .paths(paths)
            .ephemeral()
            .artifacts(ArtifactOutput::Nothing)
            .build()
            .unwrap();
        let compiled = project.compile().unwrap();
        let contracts = match compiled {
            ProjectCompileOutput::Compiled((out, _)) => {
                assert!(!out.has_error());
                out.contracts
            }
            _ => panic!("must compile"),
        };
        // Contracts A to F
        assert_eq!(contracts.keys().count(), 5);
    }

    #[test]
    #[cfg(all(feature = "svm", feature = "async"))]
    fn test_build_many_libs() {
        use super::*;

        let root = std::fs::canonicalize("./test-data/test-contract-libs").unwrap();

        let paths = ProjectPathsConfig::builder()
            .root(&root)
            .sources(root.join("src"))
            .lib(root.join("lib1"))
            .lib(root.join("lib2"))
            .build()
            .unwrap();
        let project = Project::builder()
            .paths(paths)
            .ephemeral()
            .artifacts(ArtifactOutput::Nothing)
            .build()
            .unwrap();
        let compiled = project.compile().unwrap();
        let contracts = match compiled {
            ProjectCompileOutput::Compiled((out, _)) => {
                assert!(!out.has_error());
                out.contracts
            }
            _ => panic!("must compile"),
        };
        assert_eq!(contracts.keys().count(), 3);
    }

    #[test]
    #[cfg(all(feature = "svm", feature = "async"))]
    fn test_build_remappings() {
        use super::*;

        let root = std::fs::canonicalize("./test-data/test-contract-remappings").unwrap();
        let paths = ProjectPathsConfig::builder()
            .root(&root)
            .sources(root.join("src"))
            .lib(root.join("lib"))
            .build()
            .unwrap();
        let project = Project::builder()
            .paths(paths)
            .ephemeral()
            .artifacts(ArtifactOutput::Nothing)
            .build()
            .unwrap();
        let compiled = project.compile().unwrap();
        let contracts = match compiled {
            ProjectCompileOutput::Compiled((out, _)) => {
                assert!(!out.has_error());
                out.contracts
            }
            _ => panic!("must compile"),
        };
        assert_eq!(contracts.keys().count(), 2);
    }
}
