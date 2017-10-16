#![feature(conservative_impl_trait)]

#[macro_use]
extern crate serde_derive;
extern crate docopt;
extern crate futures;
extern crate letterboxd;
extern crate regex;
extern crate tokio_core;
extern crate serde_json;

use futures::{Future, future};
use regex::Regex;

use std::collections::{HashSet, HashMap};
use std::env;
use std::fs;
use std::io;
use std::str::FromStr;

use docopt::Docopt;

const USAGE: &'static str = "
Letterboxid Sync. Synchronizes movies in a folder with a list on Letterboxd.

Usage:
    letterboxd-sync --pattern=<regex> <list-id> <folder>

Options:
    --pattern=<regex>  The pattern used to extract the movie names.
";

#[derive(Debug, Deserialize)]
struct Args {
    flag_pattern: String,
    arg_list_id: String,
    arg_folder: String,
}

/// Returns true if entry is a file, false otherwise or on error.
fn is_file(entry: &fs::DirEntry) -> bool {
    entry.metadata().ok().map(|m| m.is_file()).unwrap_or(false)
}

/// Used in to filter_map entries into files. Directories are sorted out.
fn into_file(entry: io::Result<fs::DirEntry>) -> Option<fs::DirEntry> {
    entry.ok().and_then(
        |e| if is_file(&e) { Some(e) } else { None },
    )
}

/// List all files in dir.
fn list_files(
    path: &str,
) -> Result<impl Iterator<Item = std::string::String>, Box<std::error::Error>> {
    let dir = fs::read_dir(path)?;
    let files = dir.filter_map(into_file).filter_map(|e| {
        e.file_name().into_string().ok()
    });
    Ok(files)
}

/// Search movie on letterbox.
fn search_movie(
    client: &letterboxd::Client,
    movie: std::string::String,
) -> Box<Future<Item = letterboxd::SearchResponse, Error = letterboxd::Error>> {
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
    pattern
        .captures(file_name)
        .and_then(|matches| matches.get(1))
        .and_then(|m| String::from_str(m.as_str()).ok())
}

/// Get film ids response of list entries request.
fn film_id_set_from_response(entries: Vec<letterboxd::ListEntry>) -> HashSet<String> {
    entries.into_iter().map(|entry| entry.film.id).collect()
}


fn fetch_saved_films<'a>(
    list_id: &'a str,
    client: &'a letterboxd::Client,
    token: &'a letterboxd::AccessToken,
) -> impl future::Future<Item = HashSet<String>, Error = letterboxd::Error> + 'a {

    // The state structure for the resurive loop.
    struct FetchState {
        request: letterboxd::ListEntriesRequest,
        entries: HashSet<String>,
    }

    // Decides whether to continue or break query loop.
    fn continue_or_break(
        next: Option<letterboxd::Cursor>,
        mut state: FetchState,
    ) -> future::Loop<HashSet<String>, FetchState> {
        match next {
            None => future::Loop::Break(state.entries),
            Some(cursor) => {
                state.request = letterboxd::ListEntriesRequest::default();
                state.request.cursor = Some(cursor);
                future::Loop::Continue(state)
            }
        }
    }

    let initial_state = FetchState {
        request: letterboxd::ListEntriesRequest::default(),
        entries: HashSet::new(),
    };

    // Construct actual query loop.
    future::loop_fn(initial_state, move |mut state| {
        client
            .list_entries(list_id, &state.request, Some(token))
            .map(|response| {
                state.entries.extend(
                    film_id_set_from_response(response.items),
                );
                continue_or_break(response.next, state)
            })
    })
}

fn create_update_request<I>(
    list_name: String,
    films_to_remove: Vec<String>,
    films_to_add: I,
) -> letterboxd::ListUpdateRequest
where
    I: std::iter::Iterator<Item = String>,
{
    let mut request = letterboxd::ListUpdateRequest::new(list_name);
    request.entries = films_to_add.map(letterboxd::ListUpdateEntry::new).collect();
    request.films_to_remove = films_to_remove;
    request
}

fn get_cache_filename() -> Result<std::path::PathBuf, Box<std::error::Error>> {
    const CACHE_FILENAME: &'static str = ".movies.json";
    Ok(env::current_dir()?.join(CACHE_FILENAME))
}

fn load_ids_list_from_cache() -> Result<HashMap<String, String>, Box<std::error::Error>> {
    let path = get_cache_filename()?;
    let file = fs::File::open(&path);
    let ids = match file {
        Ok(file) => {
            let ids: HashMap<String, String> = serde_json::from_reader(file)?;
            println!("Loaded {} movie ids from cache.", ids.len());
            ids
        }
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                HashMap::new()
            } else {
                return Err(Box::new(err));
            }
        }
    };
    Ok(ids)
}

fn save_ids_list_to_cache(ids: &HashMap<String, String>) -> Result<(), Box<std::error::Error>> {
    let path = &get_cache_filename()?;
    let file = fs::OpenOptions::new().write(true).create(true).open(&path)?;
    Ok(serde_json::to_writer(file, &ids)?)
}

fn sync_list(path: &str, pattern: &str, list_id: &str) -> Result<(), Box<std::error::Error>> {
    use tokio_core::reactor::Core;

    let mut core = Core::new().unwrap();
    let key = env::var("LETTERBOXD_KEY")?;
    let secret = env::var("LETTERBOXD_SECRET")?;
    let username = env::var("LETTERBOXD_USERNAME")?;
    let password = env::var("LETTERBOXD_PASSWORD")?;

    let client = letterboxd::Client::new(&core.handle(), key, secret);
    let do_auth = client.auth(&username, &password);
    let token = core.run(do_auth)?;
    println!("Got token: {:?}", token);

    let files = list_files(path)?;

    // Collect all movie names
    let re = Regex::new(pattern)?;
    let movie_names = files.filter_map(|file_name| extract_movie(&re, file_name.as_str()));

    // Load ids either from cache file or make a request to get an id
    let film_ids_cache = load_ids_list_from_cache()?;
    let film_ids_req = movie_names.map(|movie| -> Box<
        Future<
            Item = (String, String),
            Error = letterboxd::Error,
        >,
    > {
        let id = film_ids_cache.get(&movie);
        // TODO: Try to remove Box here
        if let Some(id) = id {
            Box::new(future::ok((movie, id.clone())))
        } else {
            Box::new(search_movie(&client, movie.clone()).and_then(
                move |mut resp| {
                    if resp.items.is_empty() {
                        println!("[W] Did not find id for movie: {}", movie);
                        return Ok((movie, String::new()));
                    }

                    match resp.items.drain(0..1).next() {
                        Some(letterboxd::AbstractSearchItem::FilmSearchItem { film, .. }) => {
                            println!("Resolved id of {}: {}", movie, film.id);
                            Ok((movie, film.id))
                        }
                        _ => {
                            println!("[W] Did not find id for movie: {}", movie);
                            Ok((movie, String::new()))
                        }
                    }
                },
            ))
        }
    });
    let film_ids = future::join_all(film_ids_req).map(|response| -> HashMap<String, String> {
        response
            .into_iter()
            .filter(|&(_, ref id)| !id.is_empty())
            .collect()
    });

    // Fetch ids for films already on list.
    let saved_film_ids = fetch_saved_films(list_id, &client, &token);

    // Get disjunction of films to save and films to remove.
    let to_remove_and_add = saved_film_ids.and_then(|saved| {
        film_ids.map(move |to_add| {
            if let Err(err) = save_ids_list_to_cache(&to_add) {
                println!("[W] Could not save film ids to cache: {:?}", err);
            }
            let to_add: HashSet<String> = to_add.values().cloned().collect();
            let to_remove: Vec<String> = saved.difference(&to_add).cloned().collect();
            (to_remove, to_add)
        })
    });

    // Update film list.
    let list_name = "to-watch";
    let result = to_remove_and_add
        .map(|(to_remove, to_add)| {
            create_update_request(String::from(list_name), to_remove, to_add.into_iter())
        })
        .and_then(|request| client.patch_list(list_id, &request, &token));

    println!("Result {:?}", core.run(result)?);
    Ok(())
}

fn main() {
    let args: Args = Docopt::new(USAGE)
        .and_then(|d| d.deserialize())
        .unwrap_or_else(|e| e.exit());

    sync_list(
        args.arg_folder.as_str(),
        args.flag_pattern.as_str(),
        args.arg_list_id.as_str(),
    ).expect("Error!")
}
