/*
 * Copyright 2016-2017 Doug Goldstein <cardoe@cardoe.com>
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

use std::default::Default;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context as _};
use cargo::core::{GitReference, Package, PackageId, PackageSet, Resolve, Workspace};
use cargo::{CliResult, GlobalContext};
use clap::Parser;
use itertools::Itertools;

mod git;
mod license;
mod package_info;

use package_info::PackageInfo;

const CRATES_IO_URL: &str = "crates.io";

fn get_checksum(package_set: &PackageSet, pkg_id: PackageId) -> String {
    match package_set
        .get_one(pkg_id)
        .map(|pkg| pkg.summary().checksum())
    {
        Err(_) | Ok(None) => "".to_string(),
        Ok(Some(crc)) => format!(
            ";sha256sum={crc};{}-{}.sha256sum={crc}",
            pkg_id.name(),
            pkg_id.version()
        ),
    }
}

#[derive(clap::Parser)]
struct Args {
    #[arg(short, long)]
    /// Silence all output
    quiet: bool,

    #[arg(short, action = clap::ArgAction::Count)]
    /// Verbose mode (-v, -vv, -vvv, etc.)
    verbose: u8,

    #[arg(short)]
    /// Reproducible mode: Output exact git references for git projects
    reproducible: bool,

    #[clap(short = 'c', long)]
    /// Don't emit inline checksums
    no_checksums: bool,

    #[clap(short, long)]
    /// Legacy Overrides: Use legacy override syntax
    legacy_overrides: bool,
}

#[derive(clap::Parser)]
#[command(
    name = "cargo-bitbake",
    bin_name = "cargo",
    author,
)]
enum Opt {
    /// Generates a BitBake recipe for a given Cargo project
    #[clap(name = "bitbake")]
    Bitbake(Args),
}

fn main() {
    let mut gctx = GlobalContext::default().unwrap();
    let Opt::Bitbake(args) = Opt::parse();
    let result = real_main(args, &mut gctx);

    if let Err(e) = result {
        cargo::exit_with_error(e, &mut gctx.shell());
    }
}

fn real_main(options: Args, gctx: &mut GlobalContext) -> CliResult {
    gctx.configure(
        options.verbose as u32,
        options.quiet,
        /* color */
        None,
        /* frozen */
        false,
        /* locked */
        false,
        /* offline */
        false,
        /* target dir */
        &None,
        /* unstable flags */
        &[],
        /* CLI config */
        &[],
    )?;

    // Build up data about the package we are attempting to generate a recipe for
    let md = PackageInfo::new(gctx, None)?;

    // Our current package
    let package = md.package()?;
    let crate_root = package
        .manifest_path()
        .parent()
        .expect("Cargo.toml must have a parent");

    if package.name().contains('_') {
        println!("Package name contains an underscore");
    }

    // Resolve all dependencies (generate or use Cargo.lock as necessary)
    let resolve = md.resolve()?;
    let package_set = resolve.0;

    // build the crate URIs
    let mut src_uri_extras = vec![];
    let mut src_uris = resolve
        .1
        .iter()
        .filter_map(|pkg| {
            // get the source info for this package
            let src_id = pkg.source_id();
            if pkg.name() == package.name() {
                None
            } else if src_id.is_registry() {
                // this package appears in a crate registry
                if options.no_checksums {
                    Some(format!(
                        "    crate://{}/{}/{} \\\n",
                        CRATES_IO_URL,
                        pkg.name(),
                        pkg.version()
                    ))
                } else {
                    Some(format!(
                        "    crate://{}/{}/{}{} \\\n",
                        CRATES_IO_URL,
                        pkg.name(),
                        pkg.version(),
                        get_checksum(&package_set, pkg)
                    ))
                }
            } else if src_id.is_path() {
                // we don't want to spit out path based
                // entries since they're within the crate
                // we are packaging
                None
            } else if src_id.is_git() {
                // Just use the default download method for git repositories
                // found in the source URIs, since cargo currently cannot
                // initialize submodules for git dependencies anyway.
                let url = git::git_to_yocto_git_url(
                    src_id.url().as_str(),
                    Some(pkg.name().as_str()),
                    git::GitPrefix::default(),
                );

                // save revision
                src_uri_extras.push(format!("SRCREV_FORMAT .= \"_{}\"", pkg.name()));

                let precise = if options.reproducible {
                    src_id.precise_git_fragment()
                } else {
                    None
                };

                let rev = if let Some(precise) = precise {
                    precise
                } else {
                    match *src_id.git_reference()? {
                        GitReference::Tag(ref s) => s,
                        GitReference::Rev(ref s) => {
                            if s.len() == 40 {
                                // avoid reduced hashes
                                s
                            } else {
                                let precise = src_id.precise_git_fragment();
                                if let Some(p) = precise {
                                    p
                                } else {
                                    panic!("cannot find rev in correct format!");
                                }
                            }
                        }
                        GitReference::Branch(ref s) => {
                            if s == "master" {
                                "${AUTOREV}"
                            } else {
                                s
                            }
                        }
                        GitReference::DefaultBranch => "${AUTOREV}",
                    }
                };

                src_uri_extras.push(format!("SRCREV_{} = \"{}\"", pkg.name(), rev));
                // instruct Cargo where to find this
                src_uri_extras.push(format!(
                    "EXTRA_OECARGO_PATHS += \"${{WORKDIR}}/{}\"",
                    pkg.name()
                ));

                Some(format!("    {} \\\n", url))
            } else {
                Some(format!("    {} \\\n", src_id.url()))
            }
        })
        .collect::<Vec<String>>();

    // sort the crate list
    src_uris.sort();

    // root package metadata
    let metadata = package.manifest().metadata();

    // package description is used as BitBake summary
    let summary = metadata.description.as_ref().map_or_else(
        || {
            println!("No package.description set in your Cargo.toml, using package.name");
            package.name()
        },
        |s| cargo::util::interning::InternedString::new(&s.trim().replace('\n', " \\\n")),
    );

    // package homepage (or source code location)
    let homepage = metadata
        .homepage
        .as_ref()
        .map_or_else(
            || {
                println!("No package.homepage set in your Cargo.toml, trying package.repository");
                metadata
                    .repository
                    .as_ref()
                    .ok_or_else(|| anyhow!("No package.repository set in your Cargo.toml"))
            },
            Ok,
        )?
        .trim();

    // package license
    let license = metadata.license.as_ref().map_or_else(
        || {
            println!("No package.license set in your Cargo.toml, trying package.license_file");
            metadata.license_file.as_ref().map_or_else(
                || {
                    println!("No package.license_file set in your Cargo.toml");
                    println!("Assuming {} license", license::CLOSED_LICENSE);
                    license::CLOSED_LICENSE
                },
                String::as_str,
            )
        },
        String::as_str,
    );

    // compute the relative directory into the repo our Cargo.toml is at
    let rel_dir = md.rel_dir()?;

    // license files for the package
    let mut lic_files = vec![];
    let licenses: Vec<&str> = license.split('/').collect();
    let single_license = licenses.len() == 1;
    for lic in licenses {
        lic_files.push(format!(
            "    {}",
            license::file(crate_root, &rel_dir, lic, single_license)
        ));
    }

    // license data in Yocto fmt
    let license = license.split('/').map(str::trim).join(" | ");

    // attempt to figure out the git repo for this project
    let project_repo = git::ProjectRepo::new(gctx).unwrap_or_else(|e| {
        println!("{}", e);
        Default::default()
    });

    // if this is not a tag we need to include some data about the version in PV so that
    // the sstate cache remains valid
    let git_srcpv = if !project_repo.tag && project_repo.rev.len() > 10 {
        let mut pv_append_key = "PV:append";
        // Override PV override with legacy syntax if flagged
        if options.legacy_overrides {
            pv_append_key = "PV_append";
        }
        // we should be using ${SRCPV} here but due to a bitbake bug we cannot. see:
        // https://github.com/meta-rust/meta-rust/issues/136
        format!(
            "{} = \".AUTOINC+{}\"",
            pv_append_key,
            &project_repo.rev[..10]
        )
    } else {
        // its a tag so nothing needed
        "".into()
    };

    // build up the path
    let recipe_path = PathBuf::from(format!("{}_{}.bb", package.name(), package.version()));

    // Open the file where we'll write the BitBake recipe
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&recipe_path)
        // CliResult accepts only failure::Error, not failure::Context
        .map_err(|e| anyhow!("Unable to open bitbake recipe file with: {}", e))?;

    // write the contents out
    write!(
        file,
        include_str!("bitbake.template"),
        name = package.name(),
        version = package.version(),
        summary = summary,
        homepage = homepage,
        license = license,
        lic_files = lic_files.join(""),
        src_uri = src_uris.join(""),
        src_uri_extras = src_uri_extras.join("\n"),
        project_rel_dir = rel_dir.display(),
        project_src_uri = project_repo.uri,
        project_src_rev = project_repo.rev,
        git_srcpv = git_srcpv,
        cargo_bitbake_ver = env!("CARGO_PKG_VERSION"),
    )
    .map_err(|e| anyhow!("Unable to write to bitbake recipe file with: {}", e))?;

    println!("Wrote: {}", recipe_path.display());

    Ok(())
}
