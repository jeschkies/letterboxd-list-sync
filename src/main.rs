use anyhow::{anyhow, Context as _};
use futures::{stream, StreamExt, TryStreamExt};
use log::{debug, info, warn};
use regex::Regex;
use structopt::StructOpt;

use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

const REQUESTS_CONCURRENCY: usize = 16;

/// Letterboxd Sync.
///
/// Synchronizes movies in a folder with a list on Letterboxd.
#[derive(Debug, StructOpt)]
struct Args {
    /// Disable recursive search for movies in the given folder.
    #[structopt(long)]
    no_recursive: bool,
    /// Regex pattern used to extract the movie names.
    #[structopt(long)]
    pattern: String,
    /// ID of the Letterboxd list to sync the movies with.
    list_id: String,
    /// The directory to scan movies in.
    directory: PathBuf,
    /// Do update the list at Letterboxd and do not change any data.
    #[structopt(long)]
    dry_run: bool,
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
    pub fn new(path: PathBuf, recursively: bool) -> io::Result<Files> {
        fs::read_dir(path).map(|entries| Files {
            entries,
            stack: Vec::new(),
            recursively,
        })
    }

    /// Returns the next file in the next dir on the stack if any.
    fn next_in_dir(&mut self) -> Option<String> {
        self.stack.pop().and_then(|dir| {
            match fs::read_dir(&dir) {
                Ok(new_entries) => self.entries = new_entries,
                Err(_) => warn!("Could not read files in {}", dir.display()),
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
                    warn!(
                        "Could not retrieve filename of {}",
                        filename.to_string_lossy()
                    );
                    self.next()
                }
            }
        } else if self.recursively && path.is_dir() {
            self.stack.push(path);
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
fn list_files(path: PathBuf, recursively: bool) -> anyhow::Result<Vec<String>> {
    if !path.is_dir() {
        return Ok(vec![path.display().to_string()]);
    }

    let files = Files::new(path, recursively)?;
    Ok(files.collect())
}

/// Search movie on letterbox.
async fn search_movie(
    client: &letterboxd::Client,
    movie: String,
) -> Result<letterboxd::SearchResponse, letterboxd::Error> {
    let request = letterboxd::SearchRequest {
        cursor: None,
        per_page: Some(1),
        input: movie,
        search_method: Some(letterboxd::SearchMethod::Autocomplete),
        include: None,
        contribution_type: None,
    };
    client.search(&request).await
}

/// Extract movie names from file names with given pattern.
fn extract_movie(pattern: &Regex, file_name: &str) -> Option<String> {
    let matches = pattern.captures(file_name)?;
    Some(matches.get(1)?.as_str().to_string())
}

/// Get film ids response of list entries request.
fn film_id_set_from_response(entries: Vec<letterboxd::ListEntry>) -> HashSet<String> {
    entries.into_iter().map(|entry| entry.film.id).collect()
}

async fn fetch_saved_films(
    list_id: &str,
    client: &letterboxd::Client,
) -> Result<HashSet<String>, letterboxd::Error> {
    let mut request = letterboxd::ListEntriesRequest {
        per_page: Some(100),
        ..Default::default()
    };
    let mut entries: HashSet<String> = HashSet::new();
    loop {
        let response = client.list_entries(list_id, &request).await?;
        entries.extend(film_id_set_from_response(response.items));
        request.cursor = response.next;
        if request.cursor.is_none() {
            break;
        }
    }
    Ok(entries)
}

fn get_cache_filename() -> anyhow::Result<std::path::PathBuf> {
    const CACHE_FILENAME: &str = ".movies.json";
    Ok(env::current_dir()?.join(CACHE_FILENAME))
}

fn load_ids_list_from_cache() -> anyhow::Result<HashMap<String, String>> {
    let path = get_cache_filename()?;
    let file = fs::File::open(&path);
    let ids = match file {
        Ok(file) => {
            let ids: HashMap<String, String> = serde_json::from_reader(file)?;
            debug!("Loaded {} movie ids from cache.", ids.len());
            ids
        }
        Err(err) => {
            if err.kind() == io::ErrorKind::NotFound {
                HashMap::new()
            } else {
                return Err(err.into());
            }
        }
    };
    Ok(ids)
}

fn save_ids_list_to_cache(ids: &HashMap<String, String>) -> Result<(), Box<dyn std::error::Error>> {
    let path = &get_cache_filename()?;
    let file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .open(&path)?;
    Ok(serde_json::to_writer(file, &ids)?)
}

/// Resolve movie ids from movie names by first looking in the given cache, and then, if not found,
/// by making a request through letterboxd api.
async fn resolve_film_ids(
    movie_names: impl IntoIterator<Item = String>,
    film_ids_cache: &HashMap<String, String>,
    client: &letterboxd::Client,
) -> Result<HashMap<String, String>, letterboxd::Error> {
    let film_id_requests = movie_names.into_iter().map(|movie| async {
        if let Some(id) = film_ids_cache.get(&movie) {
            Ok(Some((movie, id.clone())))
        } else {
            let response = search_movie(&client, movie.clone()).await?;
            let first_item = response.items.into_iter().next();
            match first_item {
                Some(letterboxd::AbstractSearchItem::FilmSearchItem { film, .. }) => {
                    debug!("Resolved id of {}: {}", movie, film.id);
                    Ok(Some((movie, film.id)))
                }
                _ => {
                    warn!("Did not find id for movie: {}", movie);
                    Ok(None)
                }
            }
        }
    });

    stream::iter(film_id_requests)
        .buffer_unordered(REQUESTS_CONCURRENCY)
        .filter_map(|res| std::future::ready(res.transpose()))
        .try_collect()
        .await
}

async fn new_client() -> anyhow::Result<letterboxd::Client> {
    let username = env::var("LETTERBOXD_USERNAME")
        .map_err(|_| anyhow!("missing obligatory variable LETTERBOXD_USERNAME"))?;
    let password = env::var("LETTERBOXD_PASSWORD")
        .map_err(|_| anyhow!("missing obligatory variable LETTERBOXD_PASSWORD"))?;

    let api_key_pair = letterboxd::ApiKeyPair::from_env().ok_or_else(|| {
        anyhow!(
            "No API key/secret environment variable found: \
            check if LETTERBOXD_API_KEY/LETTERBOXD_API_SECRET is set"
        )
    })?;
    // TODO: cache token
    letterboxd::Client::authenticate(api_key_pair, &username, &password)
        .await
        .context("failed to authenticate on Letterboxd")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::from_args();
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    dotenv::dotenv().ok();

    let client = new_client().await?;

    let files = list_files(args.directory, !args.no_recursive)?;

    // Collect all movie names
    let re = Regex::new(&args.pattern)?;
    let movie_names = files
        .into_iter()
        .filter_map(|file_name| extract_movie(&re, file_name.as_str()));

    // Resolve movie ids either from cache or by requesting these
    let film_ids_cache = load_ids_list_from_cache()?;
    let film_ids = resolve_film_ids(movie_names, &film_ids_cache, &client)
        .await
        .context("failed to resolve film ids")?;

    // Fetch ids for films already on list.
    let saved_film_ids = fetch_saved_films(&args.list_id, &client)
        .await
        .context("failed to fetch ids already on the list")?;

    if !args.dry_run {
        if let Err(err) = save_ids_list_to_cache(&film_ids) {
            warn!("Could not save film ids to cache: {}", err);
        }
    }

    // Get disjunction of films to save and films to remove.
    let ids: HashSet<String> = film_ids.values().cloned().collect();
    let to_add: Vec<String> = ids.difference(&saved_film_ids).cloned().collect();
    let to_remove: Vec<String> = saved_film_ids.difference(&ids).cloned().collect();

    // Update film list.
    let list_name = "Collection".to_string();
    let list_id = args.list_id.clone();
    if !to_remove.is_empty() || !to_add.is_empty() {
        let request = letterboxd::ListUpdateRequest {
            entries: to_add
                .into_iter()
                .map(letterboxd::ListUpdateEntry::new)
                .collect(),
            films_to_remove: to_remove,
            ..letterboxd::ListUpdateRequest::new(list_name)
        };
        info!(
            "Updating list: {} to add, {} to remove, total movies: {}",
            request.entries.len(),
            request.films_to_remove.len(),
            ids.len()
        );

        if !args.dry_run {
            client
                .update_list(&list_id, &request)
                .await
                .context("failed to update the list")?;
        } else {
            info!("Dry run. List was not updated.");
        }
    } else {
        info!("List up to date. Nothing to do.");
    }

    Ok(())
}
