#[macro_use]
extern crate serde_derive;
extern crate docopt;
extern crate futures;
extern crate letterboxd;
extern crate tokio_core;

use std::fs;
use std::io;

use docopt::Docopt;

const USAGE: &'static str = "
Letterboxid Sync. Synchronizes movies in a folder with a list on Letterboxd.

Usage:
    letterboxd-sync <folder>
";

#[derive(Debug, Deserialize)]
struct Args {
    arg_folder: String
}

/// Returns true if entry is a file, false otherwise or on error.
fn is_file(entry: &fs::DirEntry) -> bool {
    entry.metadata().ok()
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Used in to filter_map entries into files. Directories are sorted out.
fn into_file(entry: io::Result<fs::DirEntry>) -> Option<fs::DirEntry> {
    entry.ok().and_then(|e| {
        if is_file(&e) {
            Some(e)
        } else {
            None
        }
    })
}

/// List all files in dir.
fn list_files(dir: &str) -> Result<(), Box<std::error::Error>>{
    use tokio_core::reactor::Core;
    use futures::future;

    let mut core = Core::new().unwrap();
    let key = String::from("");
    let secret =
        String::from("");
    let client = letterboxd::Client::new(&core.handle(), key, secret);

    let dir = fs::read_dir(dir)?;
    let files = dir.filter_map(into_file)
        .filter_map(|e| e.file_name().into_string().ok());

    // Search each movie.
    let requests = files.map(|movie| {
      let request = letterboxd::SearchRequest::new(movie);
      client.search(request)
    });
    let result = future::join_all(requests);
    println!("{:?}", core.run(result)?);
    Ok(())
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    list_files(args.arg_folder.as_str()).expect("Error!")
}
