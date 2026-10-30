//! Embeds the Svelte app's build (`ui/dist` at the workspace root) for
//! `src/ui.rs`: the page, and a table of every file under `ui/dist/assets`. A
//! checkout that never ran `npm run build` gets a page saying so, so a Rust
//! build never needs Node; `PIPETTE_REQUIRE_UI` makes that an error instead, for
//! a build that ships.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs, io};

const NOT_BUILT: &str = "<!doctype html><meta charset=\"utf-8\"><title>Pipette</title>\n\
<p>This binary was built without the app. Run <code>npm ci &amp;&amp; npm run build</code> in \
<code>ui/</code>, then rebuild <code>pipette-mgmt</code>.</p>\n";

fn main() -> io::Result<()> {
    println!("cargo:rerun-if-changed=../../ui/dist");
    println!("cargo:rerun-if-env-changed=PIPETTE_REQUIRE_UI");
    let out = PathBuf::from(env::var("OUT_DIR").map_err(io::Error::other)?);
    let dist = PathBuf::from(env::var("CARGO_MANIFEST_DIR").map_err(io::Error::other)?)
        .join("../../ui/dist");

    if !dist.join("index.html").is_file() {
        if env::var_os("PIPETTE_REQUIRE_UI").is_some() {
            return Err(io::Error::other(
                "PIPETTE_REQUIRE_UI is set but ui/dist is not built: run `npm ci && npm run build` \
                 in ui/",
            ));
        }
        fs::write(out.join("index.html"), NOT_BUILT)?;
        return fs::write(out.join("ui_assets.rs"), "&[]\n");
    }

    fs::copy(dist.join("index.html"), out.join("index.html"))?;
    // Anything the build writes outside `assets/` would go unserved, so it fails here.
    for entry in fs::read_dir(&dist)? {
        let name = entry?.file_name();
        if name != "index.html" && name != "assets" {
            return Err(unserved(&dist.join(name)));
        }
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dist.join("assets"))? {
        let path = entry?.path();
        if !path.is_file() {
            return Err(unserved(&path));
        }
        files.push(path);
    }
    files.sort();

    let mut table = String::from("&[\n");
    for path in &files {
        let name = path.file_name().and_then(|n| n.to_str());
        let (Some(name), Some(full)) = (name, path.to_str()) else {
            return Err(unserved(path));
        };
        writeln!(
            table,
            "    ({name:?}, include_bytes!({full:?}).as_slice()),"
        )
        .map_err(io::Error::other)?;
    }
    table.push_str("]\n");
    fs::write(out.join("ui_assets.rs"), table)
}

fn unserved(path: &Path) -> io::Error {
    io::Error::other(format!(
        "{} would not be served: src/ui.rs serves index.html and the files directly under \
         assets/, with UTF-8 names",
        path.display()
    ))
}
