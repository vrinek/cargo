use std::collections::HashMap;

use core::PackageSet;
use util::{CargoResult, human};

// Validate that there are no duplicated native libraries among packages and
// that all packages with `links` also have a build script.
pub fn validate(deps: &PackageSet) -> CargoResult<()> {
    let mut map = HashMap::new();

    for dep in deps.iter() {
        let lib = match dep.get_manifest().get_links() {
            Some(lib) => lib,
            None => continue,
        };
        match map.get(&lib) {
            Some(previous) => {
                return Err(human(format!("native library `{}` is being linked \
                                          to by more than one package, and \
                                          can only be linked to by one \
                                          package\n\n  {}\n  {}",
                                         lib, previous, dep.get_package_id())))
            }
            None => {}
        }
        if !dep.get_manifest().get_targets().iter().any(|t| {
            t.get_profile().is_custom_build()
        }) {
            return Err(human(format!("package `{}` specifies that it links to \
                                      `{}` but does not have a custom build \
                                      script", dep.get_package_id(), lib)))
        }
        map.insert(lib, dep.get_package_id());
    }

    Ok(())
}
