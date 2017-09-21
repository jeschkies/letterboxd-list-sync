#![feature(conservative_impl_trait)]

#[macro_use]
extern crate serde_derive;
extern crate docopt;
extern crate futures;
extern crate letterboxd;
extern crate regex;
extern crate tokio_core;

use futures::{Future, future};
use regex::Regex;
use std::env;
use std::fs;
use std::io;
use std::str::FromStr;

use docopt::Docopt;

const USAGE: &'static str = "
Letterboxid Sync. Synchronizes movies in a folder with a list on Letterboxd.

Usage:
    letterboxd-sync --pattern=<regex> <folder>

Options:
    --pattern=<regex>  The pattern used to extract the movie names.
";

#[derive(Debug, Deserialize)]
struct Args {
    flag_pattern: String,
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
fn list_files(path: &str) -> Result<impl Iterator<Item = std::string::String>, Box<std::error::Error>> {
    let dir = fs::read_dir(path)?;
    let files = dir.filter_map(into_file).filter_map(|e| e.file_name().into_string().ok());
    Ok(files)
}

/// Search movie on letterbox.
fn search_movie(client: &letterboxd::Client, movie: std::string::String) -> Box<Future<Item = letterboxd::SearchResponse, Error = letterboxd::Error>> {
    let request = letterboxd::SearchRequest {
        cursor: None,
        per_page: Some(1),
        input: movie,
        search_method: Some(letterboxd::SearchMethod::Autocomplete),
        include: None,
        contribution_type: None,
    };
    client.search(&request, None)
}

/// Extract movie names from file names with given pattern.
fn extract_movie(pattern: &Regex, file_name: &str) -> Option<String> {
    pattern.captures(file_name)
        .and_then(|matches| matches.get(1))
        .and_then(|m| String::from_str(m.as_str()).ok())
}

fn sync_list(path: &str, pattern: &str) -> Result<(), Box<std::error::Error>> {
    use tokio_core::reactor::Core;

    let mut core = Core::new().unwrap();
    let key = env::var("LETTERBOXD_KEY")?;
    let secret = env::var("LETTERBOXD_SECRET")?;
    let client = letterboxd::Client::new(&core.handle(), key, secret);

    let files = list_files(path)?;

    let re = Regex::new(pattern)?;
    let movie_names = files.filter_map(|file_name| extract_movie(&re, file_name.as_str()));
    let requests = movie_names.map(|movie| search_movie(&client, movie));
    let result = future::join_all(requests);
    println!("{:?}", core.run(result)?);
    Ok(())
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    sync_list(args.arg_folder.as_str(), args.flag_pattern.as_str()).expect("Error!")
}
