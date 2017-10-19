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
use std::path::{Path, PathBuf};

use docopt::Docopt;

const USAGE: &'static str = "
Letterboxid Sync. Synchronizes movies in a folder with a list on Letterboxd.

Usage:
    letterboxd-sync [--recursive] --pattern=<regex> <list-id> <folder>

Options:
    --pattern=<regex>  The pattern used to extract the movie names.
    -r --recursive     Search for movies in the given folder recursively.
";

#[derive(Debug, Deserialize)]
struct Args {
    flag_pattern: String,
    arg_list_id: String,
    arg_folder: String,
    flag_recursive: bool,
}

/// Returns true if entry is a file, false otherwise or on error.
fn is_file(entry: &fs::DirEntry) -> bool {
    entry.metadata().ok().map(|m| m.is_file()).unwrap_or(false)
}

struct Files {
    entries: fs::ReadDir,
    stack: Vec<PathBuf>,
    recursively: bool,
}

impl Files {
    /// Returns the next file in the next dir on the stack if any.
    fn next_in_dir(&mut self) -> Option<String> {
        self.stack.pop().and_then(|dir| {
            match fs::read_dir(&dir) {
                Ok(new_entries) => self.entries = new_entries,
                Err(_) => println!("[W] Could not read files in {:?}", dir),
            }
            self.next()
        })
    }

    /// Return file of possible entry or move on to next.
    fn handle_next(&mut self, entry: fs::DirEntry) -> Option<String> {
        let path = entry.path();
        if is_file(&entry) {
            match entry.file_name().into_string() {
                Ok(filename) => Some(filename),
                Err(filename) => {
                    println!("[W] Could not retrieve filename of {:?}", filename);
                    self.next()
                }
            }
        } else if self.recursively && path.is_dir() {
            self.stack.push(path.to_path_buf());
            self.next()
        } else {
            None
        }
    }
}

impl Iterator for Files {
    type Item = String;

    fn next(&mut self) -> Option<String> {
        match self.entries.next() {
            None => self.next_in_dir(),
            Some(maybe_entry) => {
                if let Ok(entry) = maybe_entry {
                    self.handle_next(entry)
                } else {
                    self.next()
                }
            }
        }
    }
}

/// List all files in dir.
fn list_files(dir: &str, recursively: bool) -> Result<Vec<String>, Box<std::error::Error>> {
    let path = Path::new(dir);
    if !path.is_dir() {
        return Ok(vec![String::from(dir)]);
    }

    let entries = fs::read_dir(path)?;
    let files = Files {
        entries: entries,
        stack: Vec::new(),
        recursively: recursively,
    };

    Ok(files.collect())
}

/// Search movie on letterbox.
fn search_movie(
    client: &letterboxd::Client,
    movie: String,
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
                state.request.per_page = Some(100);
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

struct FilmDetails {
    name: String,
    id: Option<String>,
}

impl FilmDetails {
    fn new(name: String, id: Option<String>) -> Self {
        Self { name: name, id: id }
    }
}

impl std::iter::FromIterator<FilmDetails> for HashMap<String, String> {
    fn from_iter<I: IntoIterator<Item = FilmDetails>>(iter: I) -> Self {
        iter.into_iter()
            .filter_map(|film_details| if let Some(id) = film_details.id {
                Some((film_details.name, id))
            } else {
                None
            })
            .collect()
    }
}

/// Resolve movie ids from movie names by first looking in the given cache, and then, if not found,
/// by making a request through letterboxd api.
fn resolve_film_ids<'a, I: IntoIterator<Item = String> + 'a>(
    movie_names: I,
    film_ids_cache: &'a HashMap<String, String>,
    client: &'a letterboxd::Client,
) -> impl future::Future<Item = HashMap<String, String>, Error = letterboxd::Error> + 'a
where
{
    let film_ids_req = movie_names.into_iter().map(move |movie| -> Box<
        Future<
            Item = FilmDetails,
            Error = letterboxd::Error,
        >,
    > {
        let id = film_ids_cache.get(&movie);
        // TODO: Try to remove Box here
        if let Some(id) = id {
            Box::new(future::ok(FilmDetails::new(movie, Some(id.clone()))))
        } else {
            Box::new(search_movie(&client, movie.clone()).and_then(
                move |mut resp| {
                    if resp.items.is_empty() {
                        println!("[W] Did not find id for movie: {}", movie);
                        return Ok(FilmDetails::new(movie, None));
                    }

                    match resp.items.drain(0..1).next() {
                        Some(letterboxd::AbstractSearchItem::FilmSearchItem { film, .. }) => {
                            println!("Resolved id of {}: {}", movie, film.id);
                            Ok(FilmDetails::new(movie, Some(film.id)))
                        }
                        _ => {
                            println!("[W] Did not find id for movie: {}", movie);
                            Ok(FilmDetails::new(movie, None))
                        }
                    }
                },
            ))
        }
    });
    future::join_all(film_ids_req).map(|response| -> HashMap<String, String> {
        response.into_iter().collect()
    })
}

fn sync_list(
    path: &str,
    pattern: &str,
    list_id: &str,
    recursively: bool,
) -> Result<(), Box<std::error::Error>> {
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

    let files = list_files(path, recursively)?;

    // Collect all movie names
    let re = Regex::new(pattern)?;
    let movie_names = files.into_iter().filter_map(|file_name| {
        extract_movie(&re, file_name.as_str())
    });

    // Resolve movie ids either from cache or by requesting these
    let film_ids_cache = load_ids_list_from_cache()?;
    let film_ids = resolve_film_ids(movie_names, &film_ids_cache, &client);

    // Fetch ids for films already on list.
    let saved_film_ids = fetch_saved_films(list_id, &client, &token);

    // Get disjunction of films to save and films to remove.
    let to_remove_and_add = saved_film_ids.and_then(|saved| {
        film_ids.map(move |film_ids| {
            if let Err(err) = save_ids_list_to_cache(&film_ids) {
                println!("[W] Could not save film ids to cache: {:?}", err);
            }
            let ids: HashSet<String> = film_ids.values().cloned().collect();
            let to_add: Vec<String> = ids.difference(&saved).cloned().collect();
            let to_remove: Vec<String> = saved.difference(&ids).cloned().collect();
            (to_remove, to_add)
        })
    });

    // Update film list.
    let list_name = "Collection";
    let result = to_remove_and_add
        .map(|(to_remove, to_add)| if !to_remove.is_empty() ||
            !to_add.is_empty()
        {
            Some(create_update_request(
                String::from(list_name),
                to_remove,
                to_add.into_iter(),
            ))
        } else {
            None
        })
        .and_then(|request| if let Some(request) = request {
            println!(
                "Updating list: {} to add, {} to remove",
                request.entries.len(),
                request.films_to_remove.len()
            );
            Some(client.patch_list(list_id, &request, &token))
        } else {
            println!("List up to date. Nothing to do.");
            None
        });

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
        args.flag_recursive,
    ).expect("Error!")
}
