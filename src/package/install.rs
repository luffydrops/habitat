// Copyright (c) 2016-2017 Chef Software Inc. and/or applicable contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std;
use std::collections::{HashMap, HashSet};
use std::cmp::{Ordering, PartialOrd};
use std::env;
use std::fmt;
use std::fs::{DirEntry, File};
use std::io::prelude::*;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use toml;
use toml::Value;

use super::{Identifiable, PackageIdent, Target, PackageTarget};
use super::metadata::{Bind, BindMapping, MetaFile, PackageType, parse_key_value};
use error::{Error, Result};
use fs;

pub const DEFAULT_CFG_FILE: &'static str = "default.toml";

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PackageInstall {
    pub ident: PackageIdent,
    fs_root_path: PathBuf,
    package_root_path: PathBuf,
    pub installed_path: PathBuf,
}

// The docs recommend implementing `From` instead, but that feels a
// bit odd here.
impl Into<PackageIdent> for PackageInstall {
    fn into(self) -> PackageIdent {
        self.ident
    }
}

impl PackageInstall {
    /// Verifies an installation of a package is within the package path and returns a struct
    /// representing that package installation.
    ///
    /// Only the origin and name of a package are required - the latest version/release of a
    /// package will be returned if their optional value is not specified. If only a version is
    /// specified, the latest release of that package origin, name, and version is returned.
    ///
    /// An optional `fs_root` path may be provided to search for a package that is mounted on a
    /// filesystem not currently rooted at `/`.
    pub fn load(ident: &PackageIdent, fs_root_path: Option<&Path>) -> Result<PackageInstall> {
        let package_install = Self::resolve_package_install(ident, fs_root_path)?;
        let package_target = package_install.target()?;
        match package_target.validate() {
            Ok(()) => Ok(package_install),
            Err(e) => Err(e),
        }
    }

    /// Verifies an installation of a package that is equal or newer to a given ident and returns
    /// a Result of a `PackageIdent` if one exists.
    ///
    /// An optional `fs_root` path may be provided to search for a package that is mounted on a
    /// filesystem not currently rooted at `/`.
    pub fn load_at_least(
        ident: &PackageIdent,
        fs_root_path: Option<&Path>,
    ) -> Result<PackageInstall> {
        let package_install = Self::resolve_package_install_min(ident, fs_root_path)?;
        let package_target = package_install.target()?;
        match package_target.validate() {
            Ok(()) => Ok(package_install),
            Err(e) => Err(e),
        }
    }

    fn resolve_package_install<T>(
        ident: &PackageIdent,
        fs_root_path: Option<T>,
    ) -> Result<PackageInstall>
    where
        T: AsRef<Path>,
    {
        let fs_root_path = fs_root_path.map_or(PathBuf::from("/"), |p| p.as_ref().into());
        let package_root_path = fs::pkg_root_path(Some(&fs_root_path));
        if !package_root_path.exists() {
            return Err(Error::PackageNotFound(ident.clone()));
        }
        let pl = Self::package_list(&package_root_path)?;
        if ident.fully_qualified() {
            if pl.iter().any(|ref p| p.satisfies(ident)) {
                Ok(PackageInstall {
                    installed_path: fs::pkg_install_path(&ident, Some(&fs_root_path)),
                    fs_root_path: fs_root_path,
                    package_root_path: package_root_path,
                    ident: ident.clone(),
                })
            } else {
                Err(Error::PackageNotFound(ident.clone()))
            }
        } else {
            let latest: Option<PackageIdent> = pl.iter().filter(|&p| p.satisfies(ident)).fold(
                None,
                |winner,
                 b| {
                    match winner {
                        Some(a) => {
                            match a.partial_cmp(&b) {
                                Some(Ordering::Greater) => Some(a),
                                Some(Ordering::Equal) => Some(a),
                                Some(Ordering::Less) => Some(b.clone()),
                                None => Some(a),
                            }
                        }
                        None => Some(b.clone()),
                    }
                },
            );
            if let Some(id) = latest {
                Ok(PackageInstall {
                    installed_path: fs::pkg_install_path(&id, Some(&fs_root_path)),
                    fs_root_path: PathBuf::from(fs_root_path),
                    package_root_path: package_root_path,
                    ident: id.clone(),
                })
            } else {
                Err(Error::PackageNotFound(ident.clone()))
            }
        }
    }

    /// Find an installed package that is at minimum the version of the given ident.
    fn resolve_package_install_min<T>(
        ident: &PackageIdent,
        fs_root_path: Option<T>,
    ) -> Result<PackageInstall>
    where
        T: AsRef<Path>,
    {
        // If the PackageIndent is does not have a version, use a reasonable minimum version that
        // will be satisfied by any installed package with the same origin/name
        let ident = if None == ident.version {
            PackageIdent::new(
                ident.origin.clone(),
                ident.name.clone(),
                Some("0".into()),
                Some("0".into()),
            )
        } else {
            ident.clone()
        };
        let fs_root_path = fs_root_path.map_or(PathBuf::from("/"), |p| p.as_ref().into());
        let package_root_path = fs::pkg_root_path(Some(&fs_root_path));
        if !package_root_path.exists() {
            return Err(Error::PackageNotFound(ident.clone()));
        }

        let pl = Self::package_list(&package_root_path)?;
        let latest: Option<PackageIdent> = pl.iter()
            .filter(|ref p| p.origin == ident.origin && p.name == ident.name)
            .fold(None, |winner, b| match winner {
                Some(a) => {
                    match a.cmp(&b) {
                        Ordering::Greater | Ordering::Equal => Some(a),
                        Ordering::Less => Some(b.clone()),
                    }
                }
                None => {
                    match b.cmp(&ident) {
                        Ordering::Greater | Ordering::Equal => Some(b.clone()),
                        Ordering::Less => None,
                    }
                }
            });
        match latest {
            Some(id) => {
                Ok(PackageInstall {
                    installed_path: fs::pkg_install_path(&id, Some(&fs_root_path)),
                    fs_root_path: fs_root_path,
                    package_root_path: package_root_path,
                    ident: id.clone(),
                })
            }
            None => Err(Error::PackageNotFound(ident.clone())),
        }
    }

    pub fn new_from_parts(
        ident: PackageIdent,
        fs_root_path: PathBuf,
        package_root_path: PathBuf,
        installed_path: PathBuf,
    ) -> PackageInstall {
        PackageInstall {
            ident: ident,
            fs_root_path: fs_root_path,
            package_root_path: package_root_path,
            installed_path: installed_path,
        }
    }

    /// Determines whether or not this package has a runnable service.
    pub fn is_runnable(&self) -> bool {
        // Currently, a runnable package can be determined by checking if a `run` hook exists in
        // package's hooks directory or directly in the package prefix.
        if self.installed_path.join("hooks").join("run").is_file() ||
            self.installed_path.join("run").is_file()
        {
            true
        } else {
            false
        }
    }

    /// Determine what kind of package this is.
    pub fn pkg_type(&self) -> Result<PackageType> {
        match self.read_metafile(MetaFile::Type) {
            Ok(body) => body.parse(),
            Err(Error::MetaFileNotFound(MetaFile::Type)) => Ok(PackageType::Standalone),
            Err(e) => Err(e),
        }
    }

    /// Which services are contained in a composite package? Note that
    /// these identifiers are *as given* in the initial `plan.sh` of
    /// the composite, and not the fully-resolved identifiers you
    /// would get from other "dependency" metadata files.
    pub fn pkg_services(&self) -> Result<Vec<PackageIdent>> {
        self.read_deps(MetaFile::Services)
    }

    pub fn binds(&self) -> Result<Vec<Bind>> {
        match self.read_metafile(MetaFile::Binds) {
            Ok(body) => {
                let mut binds = Vec::new();
                for line in body.lines() {
                    match Bind::from_str(line) {
                        Ok(bind) => binds.push(bind),
                        Err(_) => return Err(Error::MetaFileMalformed(MetaFile::Binds)),
                    }
                }
                Ok(binds)
            }
            Err(Error::MetaFileNotFound(MetaFile::Binds)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    pub fn binds_optional(&self) -> Result<Vec<Bind>> {
        match self.read_metafile(MetaFile::BindsOptional) {
            Ok(body) => {
                let mut binds = Vec::new();
                for line in body.lines() {
                    match Bind::from_str(line) {
                        Ok(bind) => binds.push(bind),
                        Err(_) => return Err(Error::MetaFileMalformed(MetaFile::BindsOptional)),
                    }
                }
                Ok(binds)
            }
            Err(Error::MetaFileNotFound(MetaFile::BindsOptional)) => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    /// Returns the bind mappings for a composite package.
    pub fn bind_map(&self) -> Result<HashMap<PackageIdent, Vec<BindMapping>>> {
        match self.read_metafile(MetaFile::BindMap) {
            Ok(body) => {
                let mut bind_map = HashMap::new();
                for line in body.lines() {
                    let mut parts = line.split("=");
                    let package = match parts.next() {
                        Some(ident) => ident.parse()?,
                        None => return Err(Error::MetaFileBadBind),
                    };
                    let binds: Result<Vec<BindMapping>> = match parts.next() {
                        Some(binds) => binds.split(" ").map(|b| b.parse()).collect(),
                        None => Err(Error::MetaFileBadBind),
                    };
                    bind_map.insert(package, binds?);
                }
                Ok(bind_map)
            }
            Err(Error::MetaFileNotFound(MetaFile::BindMap)) => Ok(HashMap::new()),
            Err(e) => Err(e),
        }
    }

    /// Read and return the decoded contents of the packages default configuration.
    pub fn default_cfg(&self) -> Option<toml::value::Value> {
        match File::open(self.installed_path.join(DEFAULT_CFG_FILE)) {
            Ok(mut file) => {
                let mut raw = String::new();
                if file.read_to_string(&mut raw).is_err() {
                    return None;
                };

                match raw.parse::<Value>() {
                    Ok(v) => Some(v),
                    Err(e) => {
                        debug!("Failed to parse toml, error: {:?}", e);
                        None
                    }
                }
            }
            Err(_) => None,
        }
    }

    fn deps(&self) -> Result<Vec<PackageIdent>> {
        self.read_deps(MetaFile::Deps)
    }

    pub fn tdeps(&self) -> Result<Vec<PackageIdent>> {
        self.read_deps(MetaFile::TDeps)
    }

    /// Returns a Rust representation of the mappings defined by the `pkg_exports` plan variable.
    ///
    /// These mappings are used as a filter-map to generate a public configuration when the package
    /// is started as a service. This public configuration can be retrieved by peers to assist in
    /// configuration of themselves.
    pub fn exports(&self) -> Result<HashMap<String, String>> {
        match self.read_metafile(MetaFile::Exports) {
            Ok(body) => {
                Ok(parse_key_value(&body).map_err(|_| {
                    Error::MetaFileMalformed(MetaFile::Exports)
                })?)
            }
            Err(Error::MetaFileNotFound(MetaFile::Exports)) => Ok(HashMap::new()),
            Err(e) => Err(e),
        }
    }

    /// A vector of ports we expose
    pub fn exposes(&self) -> Result<Vec<String>> {
        match self.read_metafile(MetaFile::Exposes) {
            Ok(body) => {
                let v: Vec<String> = body.split(' ')
                    .map(|x| String::from(x.trim_right_matches('\n')))
                    .collect();
                Ok(v)
            }
            Err(Error::MetaFileNotFound(MetaFile::Exposes)) => {
                let v: Vec<String> = Vec::new();
                Ok(v)
            }
            Err(e) => Err(e),
        }
    }

    pub fn ident(&self) -> &PackageIdent {
        &self.ident
    }

    /// Returns the path elements of the package's `PATH` metafile if it exists, or an empty `Vec`
    /// if not found.
    ///
    /// If no value for `PATH` can be found, return an empty `Vec`.
    pub fn paths(&self) -> Result<Vec<PathBuf>> {
        match self.read_metafile(MetaFile::Path) {
            Ok(body) => {
                if body.is_empty() {
                    return Ok(vec![]);
                }
                // The `filter()` in this chain is to reject any path entries that do not start
                // with the package's `installed_path` (aka pkg_prefix). This check is for any
                // packages built after
                // https://github.com/habitat-sh/habitat/commit/13344a679155e5210dd58ecb9d94654f5ae676d3
                // was merged (in https://github.com/habitat-sh/habitat/pull/4067, released in
                // Habitat 0.50.0, 2017-11-30) which produced `PATH` metafiles containing extra
                // path entries.
                let pkg_prefix = fs::pkg_install_path(self.ident(), None::<&Path>);
                let v = env::split_paths(&body)
                    .filter(|p| p.starts_with(&pkg_prefix))
                    .collect();
                Ok(v)
            }
            Err(Error::MetaFileNotFound(MetaFile::Path)) => {
                if cfg!(windows) {
                    // This check is for any packages built after
                    // https://github.com/habitat-sh/habitat/commit/cc1f35e4bd9f7a8d881a602380730488e6ad055a
                    // was merged (in https://github.com/habitat-sh/habitat/pull/4478, released in
                    // Habitat 0.53.0, 2018-02-05) which stopped producing `PATH` metafiles. This
                    // workaround attempts to fallback to the `RUNTIME_ENVIRONMENT` metafile and
                    // use the value of the `PATH` key as a stand-in for the `PATH` metafile.
                    let pkg_prefix = fs::pkg_install_path(self.ident(), None::<&Path>);
                    match self.read_metafile(MetaFile::RuntimeEnvironment) {
                        Ok(ref body) => {
                            match Self::parse_runtime_environment_metafile(body)?.get("PATH") {
                                Some(env_path) => {
                                    let v = env::split_paths(env_path)
                                        .filter(|p| p.starts_with(&pkg_prefix))
                                        .collect();
                                    Ok(v)
                                }
                                None => Ok(vec![]),
                            }
                        }
                        Err(Error::MetaFileNotFound(MetaFile::RuntimeEnvironment)) => Ok(vec![]),
                        Err(e) => Err(e),
                    }
                } else {
                    Ok(vec![])
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Attempts to load the extracted package for each direct dependency and returns a
    /// `Package` struct representation of each in the returned vector.
    ///
    /// # Failures
    ///
    /// * Any direct dependency could not be located or it's contents could not be read
    ///   from disk
    fn load_deps(&self) -> Result<Vec<PackageInstall>> {
        let ddeps = self.deps()?;
        let mut deps = Vec::with_capacity(ddeps.len());
        for dep in ddeps.iter() {
            let dep_install = Self::load(dep, Some(&*self.fs_root_path))?;
            deps.push(dep_install);
        }
        Ok(deps)
    }

    /// Attempts to load the extracted package for each transitive dependency and returns a
    /// `Package` struct representation of each in the returned vector.
    ///
    /// # Failures
    ///
    /// * Any transitive dependency could not be located or it's contents could not be read
    ///   from disk
    fn load_tdeps(&self) -> Result<Vec<PackageInstall>> {
        let tdeps = self.tdeps()?;
        let mut deps = Vec::with_capacity(tdeps.len());
        for dep in tdeps.iter() {
            let dep_install = Self::load(dep, Some(&*self.fs_root_path))?;
            deps.push(dep_install);
        }
        Ok(deps)
    }

    /// Returns an ordered `Vec` of path entries which can be used to create a runtime `PATH` value
    /// when an older package is missing a `RUNTIME_ENVIRONMENT` metafile.
    ///
    /// The path is constructed by taking all `PATH` metafile entries from the current package,
    /// followed by entries from the *direct* dependencies first (in declared order), and then from
    /// any remaining transitive dependencies last (in lexically sorted order). All entries are
    /// present once in the order of their first appearance.
    ///
    /// Preserved reference implementation:
    /// https://github.com/habitat-sh/habitat/blob/333b75d6234db0531cf4a5bdcb859f7d4adc2478/components/core/src/package/install.rs#L321-L350
    fn legacy_runtime_path(&self) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();

        for p in self.paths()? {
            if seen.contains(&p) {
                continue;
            }
            seen.insert(p.clone());
            paths.push(p);
        }

        let ordered_pkgs = self.load_deps()?.into_iter().chain(
            self.load_tdeps()?.into_iter(),
        );
        for pkg in ordered_pkgs {
            for p in pkg.paths()? {
                if seen.contains(&p) {
                    continue;
                }
                seen.insert(p.clone());
                paths.push(p);
            }
        }

        Ok(paths)
    }

    fn parse_runtime_environment_metafile(body: &str) -> Result<HashMap<String, String>> {
        let mut env = HashMap::new();
        for line in body.lines() {
            let parts: Vec<&str> = line.splitn(2, "=").collect();
            if parts.len() != 2 {
                return Err(Error::MetaFileMalformed(MetaFile::RuntimeEnvironment));
            }
            let key = parts[0].to_string();
            let value = parts[1].to_string();
            env.insert(key, value);
        }
        Ok(env)
    }

    /// Return the embedded runtime environment specification for a
    /// package.
    pub fn runtime_environment(&self) -> Result<HashMap<String, String>> {
        match self.read_metafile(MetaFile::RuntimeEnvironment) {
            Ok(ref body) => Self::parse_runtime_environment_metafile(body),
            Err(Error::MetaFileNotFound(MetaFile::RuntimeEnvironment)) => {
                // If there was no RUNTIME_ENVIRONMENT, we can at
                // least return a proper PATH
                let path = env::join_paths(self.legacy_runtime_path()?.iter())?
                    .into_string()
                    .map_err(|os_string| Error::InvalidPathString(os_string))?;

                let mut env = HashMap::new();
                env.insert(String::from("PATH"), path);
                Ok(env)

            }
            Err(e) => Err(e),
        }
    }

    pub fn installed_path(&self) -> &Path {
        &*self.installed_path
    }

    /// Returns the user that the package is specified to run as
    /// or None if the package doesn't contain a SVC_USER Metafile
    pub fn svc_user(&self) -> Result<Option<String>> {
        match self.read_metafile(MetaFile::SvcUser) {
            Ok(body) => Ok(Some(body)),
            Err(Error::MetaFileNotFound(MetaFile::SvcUser)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Returns the group that the package is specified to run as
    /// or None if the package doesn't contain a SVC_GROUP Metafile
    pub fn svc_group(&self) -> Result<Option<String>> {
        match self.read_metafile(MetaFile::SvcGroup) {
            Ok(body) => Ok(Some(body)),
            Err(Error::MetaFileNotFound(MetaFile::SvcGroup)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn target(&self) -> Result<PackageTarget> {
        match self.read_metafile(MetaFile::Target) {
            Ok(body) => PackageTarget::from_str(&body),
            Err(e) => Err(e),
        }
    }

    /// Read the contents of a given metafile.
    ///
    /// # Failures
    ///
    /// * A metafile could not be found
    /// * Contents of the metafile could not be read
    /// * Contents of the metafile are unreadable or malformed
    fn read_metafile(&self, file: MetaFile) -> Result<String> {
        match self.existing_metafile(file.clone()) {
            Some(filepath) => {
                match File::open(&filepath) {
                    Ok(mut f) => {
                        let mut data = String::new();
                        if f.read_to_string(&mut data).is_err() {
                            return Err(Error::MetaFileMalformed(file));
                        }
                        Ok(data.trim().to_string())
                    }
                    Err(e) => Err(Error::MetaFileIO(e)),
                }
            }
            None => Err(Error::MetaFileNotFound(file)),
        }
    }

    /// Returns the path to a package's specified MetaFile if it exists.
    ///
    /// Useful for fallback logic for dealing with older Habitat
    /// packages.
    fn existing_metafile(&self, file: MetaFile) -> Option<PathBuf> {
        let filepath = self.installed_path.join(file.to_string());
        match std::fs::metadata(&filepath) {
            Ok(_) => Some(filepath),
            Err(_) => None,
        }
    }

    /// Reads metafiles containing dependencies represented by package identifiers separated by new
    /// lines.
    ///
    /// In most cases, we want the identifiers to be fully qualified,
    /// but in some cases (notably reading SERVICES from a composite
    /// package), they do NOT need to be fully qualified.
    ///
    /// # Failures
    ///
    /// * Contents of the metafile could not be read
    /// * Contents of the metafile are unreadable or malformed
    fn read_deps(&self, file: MetaFile) -> Result<Vec<PackageIdent>> {
        let mut deps: Vec<PackageIdent> = vec![];

        // For now, all deps files but SERVICES need fully-qualified
        // package identifiers
        let must_be_fully_qualified = {
            file != MetaFile::Services
        };

        match self.read_metafile(file) {
            Ok(body) => {
                if body.len() > 0 {
                    for id in body.lines() {
                        let package = PackageIdent::from_str(id)?;
                        if !package.fully_qualified() && must_be_fully_qualified {
                            return Err(Error::FullyQualifiedPackageIdentRequired(
                                package.to_string(),
                            ));
                        }
                        deps.push(package);
                    }
                }
                Ok(deps)
            }
            Err(Error::MetaFileNotFound(_)) => Ok(deps),
            Err(e) => Err(e),
        }
    }

    /// Returns a list of package structs built from the contents of the given directory.
    fn package_list(path: &Path) -> Result<Vec<PackageIdent>> {
        let mut package_list: Vec<PackageIdent> = vec![];
        if std::fs::metadata(path)?.is_dir() {
            Self::walk_origins(&path, &mut package_list)?;
        }
        Ok(package_list)
    }

    /// Helper function for package_list. Walks the given path for origin directories
    /// and builds on the given package list by recursing into name, version, and release
    /// directories.
    fn walk_origins(path: &Path, packages: &mut Vec<PackageIdent>) -> Result<()> {
        for entry in std::fs::read_dir(path)? {
            let origin = entry?;
            if std::fs::metadata(origin.path())?.is_dir() {
                Self::walk_names(&origin, packages)?;
            }
        }
        Ok(())
    }

    /// Helper function for walk_origins. Walks the given origin DirEntry for name
    /// directories and recurses into them to find version and release directories.
    fn walk_names(origin: &DirEntry, packages: &mut Vec<PackageIdent>) -> Result<()> {
        for name in std::fs::read_dir(origin.path())? {
            let name = name?;
            let origin = origin
                .file_name()
                .to_string_lossy()
                .into_owned()
                .to_string();
            if std::fs::metadata(name.path())?.is_dir() {
                Self::walk_versions(&origin, &name, packages)?;
            }
        }
        Ok(())
    }

    /// Helper function for walk_names. Walks the given name DirEntry for directories and recurses
    /// into them to find release directories.
    fn walk_versions(
        origin: &String,
        name: &DirEntry,
        packages: &mut Vec<PackageIdent>,
    ) -> Result<()> {
        for version in std::fs::read_dir(name.path())? {
            let version = version?;
            let name = name.file_name().to_string_lossy().into_owned().to_string();
            if std::fs::metadata(version.path())?.is_dir() {
                Self::walk_releases(origin, &name, &version, packages)?;
            }
        }
        Ok(())
    }

    /// Helper function for walk_versions. Walks the given release DirEntry for directories and
    /// recurses into them to find version directories. Finally, a Package struct is built and
    /// concatenated onto the given packages vector with the origin, name, version, and release of
    /// each.
    fn walk_releases(
        origin: &String,
        name: &String,
        version: &DirEntry,
        packages: &mut Vec<PackageIdent>,
    ) -> Result<()> {
        for release in std::fs::read_dir(version.path())? {
            let release = release?
                .file_name()
                .to_string_lossy()
                .into_owned()
                .to_string();
            let version = version
                .file_name()
                .to_string_lossy()
                .into_owned()
                .to_string();
            let ident =
                PackageIdent::new(origin.clone(), name.clone(), Some(version), Some(release));
            packages.push(ident)
        }
        Ok(())
    }
}

impl fmt::Display for PackageInstall {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.ident)
    }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Write;

    use tempdir::TempDir;
    use time;
    use toml;

    use super::*;
    use package::test_support::fixture_path;

    /// Creates a minimal installed package under an fs_root and return a corresponding loaded
    /// `PackageInstall` suitable for testing against. The `IDENT` and `TARGET` metafiles are
    /// created and for the target system the tests are running on. Further subdirectories, files,
    /// and metafile can be created under this path.
    fn testing_package_install(ident: &str, fs_root: &Path) -> PackageInstall {
        fn write_file(path: &Path, content: &str) {
            let mut f = File::create(path).unwrap();
            f.write_all(content.as_bytes()).unwrap()
        }

        let mut pkg_ident = PackageIdent::from_str(ident).unwrap();
        if !pkg_ident.fully_qualified() {
            if let None = pkg_ident.version {
                pkg_ident.version = Some(String::from("1.0.0"));
            }
            if let None = pkg_ident.release {
                pkg_ident.release = Some(
                    time::now_utc()
                        .strftime("%Y%m%d%H%M%S")
                        .unwrap()
                        .to_string(),
                );
            }
        }
        let pkg_install_path = fs::pkg_install_path(&pkg_ident, Some(fs_root));

        std::fs::create_dir_all(&pkg_install_path).unwrap();
        write_file(
            &pkg_install_path.join(MetaFile::Ident.to_string()),
            &pkg_ident.to_string(),
        );
        write_file(
            &pkg_install_path.join(MetaFile::Target.to_string()),
            &PackageTarget::default().to_string(),
        );

        PackageInstall::load(&pkg_ident, Some(fs_root)).expect(
            &format!(
                "PackageInstall should load for {}",
                &pkg_ident
            ),
        )
    }

    /// Write the given contents into the specified metadata file for
    /// the package.
    fn write_metafile(pkg_install: &PackageInstall, metafile: MetaFile, content: &str) {
        let path = pkg_install.installed_path().join(metafile.to_string());
        let mut f = File::create(path).expect("Could not create metafile");
        f.write_all(content.as_bytes()).expect(
            "Could not write metafile contents",
        );
    }

    /// Creates a `PATH` metafile with path entries all prefixed with the package's `pkg_prefix`.
    fn set_path_for(pkg_install: &PackageInstall, paths: Vec<&str>) {
        write_metafile(
            &pkg_install,
            MetaFile::Path,
            env::join_paths(paths.iter().map(|p| pkg_prefix_for(pkg_install).join(p)))
                .unwrap()
                .to_string_lossy()
                .as_ref(),
        );
    }

    /// Returns the prefix path for a `PackageInstall`, making sure to not include any `FS_ROOT`.
    fn pkg_prefix_for(pkg_install: &PackageInstall) -> PathBuf {
        fs::pkg_install_path(pkg_install.ident(), None::<&Path>)
    }

    #[test]
    fn can_serialize_default_config() {
        let package_ident = PackageIdent::from_str("just/nothing").unwrap();
        let fixture_path = fixture_path("test_package");
        let package_install = PackageInstall {
            ident: package_ident,
            fs_root_path: PathBuf::from(""),
            package_root_path: PathBuf::from(""),
            installed_path: fixture_path,
        };

        let cfg = package_install.default_cfg().unwrap();

        match toml::ser::to_string(&cfg) {
            Ok(_) => (),
            Err(e) => assert!(false, format!("{:?}", e)),
        }
    }

    #[test]
    fn reading_a_valid_bind_map_file_works() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let package_install = testing_package_install("core/composite", fs_root.path());

        // Create a BIND_MAP file for that package
        let bind_map_contents = r#"
core/foo=db:core/database fe:core/front-end be:core/back-end
core/bar=pub:core/publish sub:core/subscribe
        "#;
        write_metafile(&package_install, MetaFile::BindMap, bind_map_contents);

        // Grab the bind map from that package
        let bind_map = package_install.bind_map().unwrap();

        // Assert that it was interpreted correctly
        let mut expected: HashMap<PackageIdent, Vec<BindMapping>> = HashMap::new();
        expected.insert(
            "core/foo".parse().unwrap(),
            vec![
                "db:core/database".parse().unwrap(),
                "fe:core/front-end".parse().unwrap(),
                "be:core/back-end".parse().unwrap(),
            ],
        );
        expected.insert(
            "core/bar".parse().unwrap(),
            vec![
                "pub:core/publish".parse().unwrap(),
                "sub:core/subscribe".parse().unwrap(),
            ],
        );

        assert_eq!(expected, bind_map);
    }

    #[test]
    fn reading_a_bad_bind_map_file_results_in_an_error() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let package_install = testing_package_install("core/dud", fs_root.path());

        // Create a BIND_MAP directory for that package
        let bind_map_contents = "core/foo=db:this-is-not-an-identifier";
        write_metafile(&package_install, MetaFile::BindMap, bind_map_contents);

        // Grab the bind map from that package
        let bind_map = package_install.bind_map();
        assert!(bind_map.is_err());
    }

    /// Composite packages don't need to have a BIND_MAP file, and
    /// standalone packages will never have them. This is OK.
    #[test]
    fn missing_bind_map_files_are_ok() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let package_install = testing_package_install("core/no-binds", fs_root.path());

        // Grab the bind map from that package
        let bind_map = package_install.bind_map().unwrap();
        assert!(bind_map.is_empty());

    }

    #[test]
    fn paths_metafile_single() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());
        set_path_for(&pkg_install, vec!["bin"]);

        assert_eq!(
            vec![pkg_prefix_for(&pkg_install).join("bin")],
            pkg_install.paths().unwrap()
        );
    }

    #[test]
    fn paths_metafile_multiple() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());
        set_path_for(&pkg_install, vec!["bin", "sbin", ".gem/bin"]);

        let pkg_prefix = pkg_prefix_for(&pkg_install);

        assert_eq!(
            vec![
                pkg_prefix.join("bin"),
                pkg_prefix.join("sbin"),
                pkg_prefix.join(".gem/bin"),
            ],
            pkg_install.paths().unwrap()
        );
    }

    #[test]
    fn paths_metafile_missing() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());

        assert_eq!(Vec::<PathBuf>::new(), pkg_install.paths().unwrap());
    }

    #[test]
    fn paths_metafile_empty() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());

        // Create a zero-sizd `PATH` metafile
        let _ = File::create(pkg_install.installed_path.join(MetaFile::Path.to_string())).unwrap();

        assert_eq!(Vec::<PathBuf>::new(), pkg_install.paths().unwrap());
    }

    #[test]
    fn paths_metafile_drop_extra_entries() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());
        let other_pkg_install = testing_package_install("acme/prophets-of-rage", fs_root.path());

        // Create `PATH` metafile which has path entries from another package to replicate certain
        // older packages
        write_metafile(
            &pkg_install,
            MetaFile::Path,
            env::join_paths(
                vec![
                    pkg_prefix_for(&pkg_install).join("bin"),
                    pkg_prefix_for(&other_pkg_install).join("bin"),
                    pkg_prefix_for(&other_pkg_install).join("sbin"),
                ].iter(),
            ).unwrap()
                .to_string_lossy()
                .as_ref(),
        );

        assert_eq!(
            vec![pkg_prefix_for(&pkg_install).join("bin")],
            pkg_install.paths().unwrap()
        );
    }

    #[cfg(windows)]
    #[test]
    fn win_legacy_paths_metafile_missing_with_runtime_metafile() {
        let fs_root = TempDir::new("fs-root").unwrap();
        let pkg_install = testing_package_install("acme/pathy", fs_root.path());
        let other_pkg_install = testing_package_install("acme/prophets-of-rage", fs_root.path());

        // Create `RUNTIME_ENVIROMENT` metafile which has path entries from another package to
        // replicate certain older packages
        let path_val = env::join_paths(
            vec![
                pkg_prefix_for(&pkg_install).join("bin"),
                pkg_prefix_for(&other_pkg_install).join("bin"),
                pkg_prefix_for(&other_pkg_install).join("sbin"),
            ].iter(),
        ).unwrap();
        write_metafile(
            &pkg_install,
            MetaFile::RuntimeEnvironment,
            &format!("PATH={}\n", path_val.to_string_lossy().as_ref()),
        );

        assert_eq!(
            vec![pkg_prefix_for(&pkg_install).join("bin")],
            pkg_install.paths().unwrap()
        );
    }

    // This test ensures the correct ordering of runtime `PATH` entries for legacy packages which
    // lack a `RUNTIME_ENVIRONMENT` metafile.
    #[test]
    fn legacy_runtime_path() {
        fn set_deps_for(pkg_install: &PackageInstall, deps: Vec<&PackageInstall>) {
            let mut content = String::new();
            for dep in deps.iter().map(|d| d.ident()) {
                content.push_str(&format!("{}\n", dep));
            }
            write_metafile(&pkg_install, MetaFile::Deps, &content);
        }

        fn set_tdeps_for(pkg_install: &PackageInstall, tdeps: Vec<&PackageInstall>) {
            let mut content = String::new();
            for tdep in tdeps.iter().map(|d| d.ident()) {
                content.push_str(&format!("{}\n", tdep));
            }
            write_metafile(&pkg_install, MetaFile::TDeps, &content);
        }

        fn paths_for(pkg_install: &PackageInstall) -> Vec<PathBuf> {
            pkg_install.paths().unwrap()
        }

        let fs_root = TempDir::new("fs-root").unwrap();

        let hotel = testing_package_install("acme/hotel", fs_root.path());
        set_path_for(&hotel, vec!["bin"]);

        let golf = testing_package_install("acme/golf", fs_root.path());
        set_path_for(&golf, vec!["bin"]);

        let foxtrot = testing_package_install("acme/foxtrot", fs_root.path());
        set_path_for(&foxtrot, vec!["bin"]);

        let echo = testing_package_install("acme/echo", fs_root.path());
        set_deps_for(&echo, vec![&foxtrot]);
        set_tdeps_for(&echo, vec![&foxtrot]);

        let delta = testing_package_install("acme/delta", fs_root.path());
        set_deps_for(&delta, vec![&echo]);
        set_tdeps_for(&delta, vec![&echo, &foxtrot]);

        let charlie = testing_package_install("acme/charlie", fs_root.path());
        set_path_for(&charlie, vec!["sbin"]);
        set_deps_for(&charlie, vec![&golf, &delta]);
        set_tdeps_for(&charlie, vec![&delta, &echo, &foxtrot, &golf]);

        let beta = testing_package_install("acme/beta", fs_root.path());
        set_path_for(&beta, vec!["bin"]);
        set_deps_for(&beta, vec![&delta]);
        set_tdeps_for(&beta, vec![&delta, &echo, &foxtrot]);

        let alpha = testing_package_install("acme/alpha", fs_root.path());
        set_path_for(&alpha, vec!["sbin", ".gem/bin", "bin"]);
        set_deps_for(&alpha, vec![&charlie, &hotel, &beta]);
        set_tdeps_for(
            &alpha,
            vec![&beta, &charlie, &delta, &echo, &foxtrot, &golf, &hotel],
        );

        let mut expected = Vec::new();
        expected.append(&mut paths_for(&alpha));
        expected.append(&mut paths_for(&charlie));
        expected.append(&mut paths_for(&hotel));
        expected.append(&mut paths_for(&beta));
        expected.append(&mut paths_for(&foxtrot));
        expected.append(&mut paths_for(&golf));

        assert_eq!(expected, alpha.legacy_runtime_path().unwrap());
    }
}
