use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use cargo::core::registry::PackageRegistry;
use cargo::core::resolver::{CliFeatures, HasDevUnits};
use cargo::core::{Package, PackageSet, Resolve, Workspace};
use cargo::util::important_paths;
use cargo::{ops, CargoResult, GlobalContext};

/// Represents the package we are trying to generate a recipe for
pub(crate) struct PackageInfo<'gctx> {
    pub(crate) _gctx: &'gctx GlobalContext,
    pub(crate) current_manifest: PathBuf,
    pub(crate) ws: Workspace<'gctx>,
}

impl<'gctx> PackageInfo<'gctx> {
    /// creates our package info from the global context and the
    /// `manifest_path`, which may not be provided
    pub(crate) fn new(gctx: &GlobalContext, manifest_path: Option<String>) -> CargoResult<PackageInfo> {
        let manifest_path = manifest_path.map_or_else(|| gctx.cwd().to_path_buf(), PathBuf::from);
        let root = important_paths::find_root_manifest_for_wd(&manifest_path)?;
        let ws = Workspace::new(&root, gctx)?;
        Ok(PackageInfo {
            _gctx: gctx,
            current_manifest: root,
            ws,
        })
    }

    /// provides the current package we are working with
    pub(crate) fn package(&self) -> CargoResult<&Package> {
        self.ws.current()
    }

    /// Generates a package registry by using the Cargo.lock or
    /// creating one as necessary
    pub(crate) fn registry(&self) -> CargoResult<PackageRegistry<'gctx>> {
        let mut registry = self.ws.package_registry()?;
        let package = self.package()?;
        registry.add_sources(vec![package.package_id().source_id()])?;
        Ok(registry)
    }

    /// Resolve the packages necessary for the workspace
    pub(crate) fn resolve(&self) -> CargoResult<(PackageSet<'gctx>, Resolve)> {
        // build up our registry
        let mut registry = self.registry()?;

        // resolve our dependencies
        let dry_run = false;
        let (packages, resolve) = ops::resolve_ws(&self.ws, dry_run)?;

        // resolve with all features set so we ensure we get all of the depends downloaded
        let resolve = ops::resolve_with_previous(
            &mut registry,
            &self.ws,
            /* resolve it all */
            &CliFeatures::new_all(true),
            HasDevUnits::No,
            /* previous */
            Some(&resolve),
            /* don't avoid any */
            None,
            /* specs */
            &[],
            /* warn? */
            true,
        )?;

        Ok((packages, resolve))
    }

    /// packages that are part of a workspace are a sub directory from the
    /// top level which we need to record, this provides us with that
    /// relative directory
    pub(crate) fn rel_dir(&self) -> CargoResult<PathBuf> {
        // this is the top level of the workspace
        let root = self.ws.root().to_path_buf();
        // path where our current package's Cargo.toml lives
        let cwd = self.current_manifest.parent().ok_or_else(|| {
            anyhow!(
                "Could not get parent of directory '{}'",
                self.current_manifest.display()
            )
        })?;

        cwd.strip_prefix(&root)
            .map(Path::to_path_buf)
            .context("Unable to if Cargo.toml is in a sub directory")
    }
}
