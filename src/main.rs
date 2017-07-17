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

    let core = Core::new().unwrap();
    let key = String::from("4a168ac5ef7f124d03364db8be04394f319a4114a2e70695fa585ef778dd15e6");
    let secret =
        String::from("27be8dfc7d2c27e8cffb0b74a8e5c9235e70c71f6c34892677bd6746fbcc0b0b");
    let client = letterboxd::Client::new(&core.handle(), key, secret);

    let dir = fs::read_dir(dir)?;
    let files = dir.filter_map(into_file)
        .filter_map(|e| e.file_name().into_string().ok());

    // Search each movie.
    let requests = files.map(|movie| {
      let request = letterboxd::SearchRequest::new(movie);
      client.search(request)
    });
    future::join_all(requests);
//    for file in files {
//        println!("{:?}", file);
//    }
    Ok(())
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    list_files(args.arg_folder.as_str()).expect("Error!")
}
