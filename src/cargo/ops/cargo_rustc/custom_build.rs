use std::collections::HashMap;
use std::ffi::CString;
use std::old_io::fs::PathExtensions;
use std::old_io::{fs, USER_RWX, File};
use std::str;
use std::sync::Mutex;

use core::{Package, Target, PackageId, PackageSet};
use util::{CargoResult, human, Human};
use util::{internal, ChainError};

use super::job::Work;
use super::{fingerprint, process, Kind, Context, Platform};
use super::CommandType;
use util::Freshness;

/// Contains the parsed output of a custom build script.
#[derive(Clone, Debug)]
pub struct BuildOutput {
    /// Paths to pass to rustc with the `-L` flag
    pub library_paths: Vec<Path>,
    /// Names and link kinds of libraries, suitable for the `-l` flag
    pub library_links: Vec<String>,
    /// Metadata to pass to the immediate dependencies
    pub metadata: Vec<(String, String)>,
}

pub type BuildMap = HashMap<(PackageId, Kind), BuildOutput>;

pub struct BuildState {
    pub outputs: Mutex<BuildMap>,
}

/// Prepares a `Work` that executes the target as a custom build script.
///
/// The `req` given is the requirement which this run of the build script will
/// prepare work for. If the requirement is specified as both the target and the
/// host platforms it is assumed that the two are equal and the build script is
/// only run once (not twice).
pub fn prepare(pkg: &Package, target: &Target, req: Platform,
               cx: &mut Context) -> CargoResult<(Work, Work, Freshness)> {
    let kind = match req { Platform::Plugin => Kind::Host, _ => Kind::Target, };
    let (script_output, build_output) = {
        (cx.layout(pkg, Kind::Host).build(pkg),
         cx.layout(pkg, kind).build_out(pkg))
    };

    // Building the command to execute
    let to_exec = try!(cx.target_filenames(target))[0].clone();
    let to_exec = script_output.join(to_exec);

    // Start preparing the process to execute, starting out with some
    // environment variables.
    let profile = target.get_profile();
    let to_exec = CString::from_slice(to_exec.as_vec());
    let p = try!(super::process(CommandType::Host(to_exec), pkg, target, cx));
    let mut p = p.env("OUT_DIR", Some(&build_output))
                 .env("CARGO_MANIFEST_DIR", Some(pkg.get_manifest_path()
                                                    .dir_path()
                                                    .display().to_string()))
                 .env("NUM_JOBS", Some(cx.jobs().to_string()))
                 .env("TARGET", Some(match kind {
                     Kind::Host => cx.config.rustc_host(),
                     Kind::Target => cx.target_triple(),
                 }))
                 .env("DEBUG", Some(profile.get_debug().to_string()))
                 .env("OPT_LEVEL", Some(profile.get_opt_level().to_string()))
                 .env("PROFILE", Some(profile.get_env()))
                 .env("HOST", Some(cx.config.rustc_host()));

    // Be sure to pass along all enabled features for this package, this is the
    // last piece of statically known information that we have.
    match cx.resolve.features(pkg.get_package_id()) {
        Some(features) => {
            for feat in features.iter() {
                p = p.env(format!("CARGO_FEATURE_{}",
                                  super::envify(feat.as_slice())).as_slice(),
                          Some("1"));
            }
        }
        None => {}
    }

    // Gather the set of native dependencies that this package has along with
    // some other variables to close over.
    //
    // This information will be used at build-time later on to figure out which
    // sorts of variables need to be discovered at that time.
    let lib_deps = {
        let non_build_target = pkg.get_targets().iter().find(|t| {
            !t.get_profile().is_custom_build()
        }).unwrap();
        cx.dep_targets(pkg, non_build_target).iter().filter_map(|&(pkg, _)| {
            pkg.get_manifest().get_links().map(|links| {
                (links.to_string(), pkg.get_package_id().clone())
            })
        }).collect::<Vec<_>>()
    };
    let pkg_name = pkg.to_string();
    let build_state = cx.build_state.clone();
    let id = pkg.get_package_id().clone();
    let all = (id.clone(), pkg_name.clone(), build_state.clone(),
               build_output.clone());
    let plugin_deps = super::crawl_build_deps(cx, pkg, target, Kind::Host);

    try!(fs::mkdir_recursive(&cx.layout(pkg, Kind::Target).build(pkg), USER_RWX));
    try!(fs::mkdir_recursive(&cx.layout(pkg, Kind::Host).build(pkg), USER_RWX));

    let exec_engine = cx.exec_engine.clone();

    // Prepare the unit of "dirty work" which will actually run the custom build
    // command.
    //
    // Note that this has to do some extra work just before running the command
    // to determine extra environment variables and such.
    let work = Work::new(move |desc_tx| {
        // Make sure that OUT_DIR exists.
        //
        // If we have an old build directory, then just move it into place,
        // otherwise create it!
        if !build_output.exists() {
            try!(fs::mkdir(&build_output, USER_RWX).chain_error(|| {
                internal("failed to create script output directory for \
                          build command")
            }));
        }

        // For all our native lib dependencies, pick up their metadata to pass
        // along to this custom build command. We're also careful to augment our
        // dynamic library search path in case the build script depended on any
        // native dynamic libraries.
        let mut p = p;
        {
            let build_state = build_state.outputs.lock().unwrap();
            for &(ref name, ref id) in lib_deps.iter() {
                let data = &build_state[(id.clone(), kind)].metadata;
                for &(ref key, ref value) in data.iter() {
                    p = p.env(format!("DEP_{}_{}",
                                      super::envify(name.as_slice()),
                                      super::envify(key.as_slice())).as_slice(),
                              Some(value.as_slice()));
                }
            }
            p = try!(super::add_plugin_deps(p, &build_state, plugin_deps));
        }

        // And now finally, run the build command itself!
        desc_tx.send(p.to_string()).ok();
        let output = try!(exec_engine.exec_with_output(p).map_err(|mut e| {
            e.desc = format!("failed to run custom build command for `{}`\n{}",
                             pkg_name, e.desc);
            Human(e)
        }));

        // After the build command has finished running, we need to be sure to
        // remember all of its output so we can later discover precisely what it
        // was, even if we don't run the build command again (due to freshness).
        //
        // This is also the location where we provide feedback into the build
        // state informing what variables were discovered via our script as
        // well.
        let output = try!(str::from_utf8(output.output.as_slice()).chain_error(|| {
            human("build script output was not valid utf-8")
        }));
        let parsed_output = try!(BuildOutput::parse(output, pkg_name.as_slice()));
        build_state.insert(id, req, parsed_output);

        try!(File::create(&build_output.dir_path().join("output"))
                  .write_str(output).map_err(|e| {
            human(format!("failed to write output of custom build command: {}",
                          e))
        }));

        Ok(())
    });

    // Now that we've prepared our work-to-do, we need to prepare the fresh work
    // itself to run when we actually end up just discarding what we calculated
    // above.
    //
    // Note that the freshness calculation here is the build_cmd freshness, not
    // target specific freshness. This is because we don't actually know what
    // the inputs are to this command!
    //
    // Also note that a fresh build command needs to
    let (freshness, dirty, fresh) =
            try!(fingerprint::prepare_build_cmd(cx, pkg, kind, Some(target)));
    let dirty = Work::new(move |tx| {
        try!(work.call((tx.clone())));
        dirty.call(tx)
    });
    let fresh = Work::new(move |tx| {
        let (id, pkg_name, build_state, build_output) = all;
        let new_loc = build_output.dir_path().join("output");
        let mut f = try!(File::open(&new_loc).map_err(|e| {
            human(format!("failed to read cached build command output: {}", e))
        }));
        let contents = try!(f.read_to_string());
        let output = try!(BuildOutput::parse(contents.as_slice(),
                                             pkg_name.as_slice()));
        build_state.insert(id, req, output);

        fresh.call(tx)
    });

    Ok((dirty, fresh, freshness))
}

impl BuildState {
    pub fn new(config: super::BuildConfig,
               packages: &PackageSet) -> BuildState {
        let mut sources = HashMap::new();
        for package in packages.iter() {
            match package.get_manifest().get_links() {
                Some(links) => {
                    sources.insert(links.to_string(),
                                   package.get_package_id().clone());
                }
                None => {}
            }
        }
        let mut outputs = HashMap::new();
        let i1 = config.host.overrides.into_iter().map(|p| (p, Kind::Host));
        let i2 = config.target.overrides.into_iter().map(|p| (p, Kind::Target));
        for ((name, output), kind) in i1.chain(i2) {
            match sources.get(&name) {
                Some(id) => { outputs.insert((id.clone(), kind), output); }

                // If no package is using the library named `name`, then this is
                // just an override that we ignore.
                None => {}
            }
        }
        BuildState { outputs: Mutex::new(outputs) }
    }

    fn insert(&self, id: PackageId, req: Platform,
              output: BuildOutput) {
        let mut outputs = self.outputs.lock().unwrap();
        match req {
            Platform::Target => { outputs.insert((id, Kind::Target), output); }
            Platform::Plugin => { outputs.insert((id, Kind::Host), output); }

            // If this build output was for both the host and target platforms,
            // we need to insert it at both places.
            Platform::PluginAndTarget => {
                outputs.insert((id.clone(), Kind::Host), output.clone());
                outputs.insert((id, Kind::Target), output);
            }
        }
    }
}

impl BuildOutput {
    // Parses the output of a script.
    // The `pkg_name` is used for error messages.
    pub fn parse(input: &str, pkg_name: &str) -> CargoResult<BuildOutput> {
        let mut library_paths = Vec::new();
        let mut library_links = Vec::new();
        let mut metadata = Vec::new();
        let whence = format!("build script of `{}`", pkg_name);

        for line in input.lines() {
            let mut iter = line.splitn(1, |&: c: char| c == ':');
            if iter.next() != Some("cargo") {
                // skip this line since it doesn't start with "cargo:"
                continue;
            }
            let data = match iter.next() {
                Some(val) => val,
                None => continue
            };

            // getting the `key=value` part of the line
            let mut iter = data.splitn(1, |&: c: char| c == '=');
            let key = iter.next();
            let value = iter.next();
            let (key, value) = match (key, value) {
                (Some(a), Some(b)) => (a, b.trim_right()),
                // line started with `cargo:` but didn't match `key=value`
                _ => return Err(human(format!("Wrong output in {}: `{}`",
                                              whence, line)))
            };

            if key == "rustc-flags" {
                let whence = whence.as_slice();
                let (libs, links) = try!(
                    BuildOutput::parse_rustc_flags(value, whence)
                );
                library_links.extend(links.into_iter());
                library_paths.extend(libs.into_iter());
            } else {
                metadata.push((key.to_string(), value.to_string()))
            }
        }

        Ok(BuildOutput {
            library_paths: library_paths,
            library_links: library_links,
            metadata: metadata,
        })
    }

    pub fn parse_rustc_flags(value: &str, whence: &str)
                             -> CargoResult<(Vec<Path>, Vec<String>)> {
        // TODO: some arguments (like paths) may contain spaces
        let value = value.trim();
        let mut flags_iter = value.words();
        let (mut library_links, mut library_paths) = (Vec::new(), Vec::new());
        loop {
            let flag = match flags_iter.next() {
                Some(f) => f,
                None => break
            };
            if flag != "-l" && flag != "-L" {
                return Err(human(format!("Only `-l` and `-L` flags are allowed \
                                         in {}: `{}`",
                                         whence, value)))
            }
            let value = match flags_iter.next() {
                Some(v) => v,
                None => return Err(human(format!("Flag in rustc-flags has no value \
                                                  in {}: `{}`",
                                                  whence, value)))
            };
            match flag {
                "-l" => library_links.push(value.to_string()),
                "-L" => library_paths.push(Path::new(value)),

                // was already checked above
                _ => return Err(human("only -l and -L flags are allowed"))
            };
        }
        Ok((library_paths, library_links))
    }
}
