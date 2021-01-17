use anyhow::{anyhow, Context as _};
use futures_util::{stream, StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use log::{debug, info, warn};
use regex::Regex;
use structopt::StructOpt;
use walkdir::{DirEntry, WalkDir};

use std::collections::{HashMap, HashSet};
use std::env;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

const REQUESTS_CONCURRENCY: usize = 16;
const TITLE_YEAR_RE: &str = r"(?P<t>.*?)(?:\((\d{4}).*\)|\[(\d{4}).*\]|\.(\d{4}).*\.| (\d{4}) )";

/// Letterboxd Sync.
///
/// Synchronizes movies in a folder with a list on Letterboxd.
#[derive(Debug, StructOpt)]
struct Args {
    /// Disable recursive search for movies in the given folder.
    #[structopt(long)]
    no_recursive: bool,
    /// ID of the Letterboxd list to sync the movies with.
    list_id: String,
    /// The directory to scan movies in.
    directory: PathBuf,
    /// Do NOT update the list at Letterboxd.
    #[structopt(long)]
    dry_run: bool,
}

/// List all movie files in a dir.
fn list_movie_files(path: PathBuf, recursively: bool) -> walkdir::Result<Vec<DirEntry>> {
    const ACCEPTED_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "m4v"];

    fn is_hidden(entry: &DirEntry) -> bool {
        entry
            .file_name()
            .to_str()
            .map(|s| s != "." && s.starts_with('.'))
            .unwrap_or(false)
    }

    fn is_accepted_file(entry: &DirEntry) -> bool {
        !entry.file_type().is_file()
            || entry
                .path()
                .extension()
                .and_then(|ext| ext.to_str())
                .map(|ext| ACCEPTED_EXTENSIONS.contains(&ext))
                .unwrap_or(false)
    }

    let mut walker = WalkDir::new(path);
    if !recursively {
        walker = walker.max_depth(0);
    }
    walker
        .into_iter()
        .filter_entry(|e| !is_hidden(e) && is_accepted_file(e))
        .filter_map(|res| {
            res.map(|e| Some(e).filter(|e| e.file_type().is_file()))
                .transpose()
        })
        .collect()
}

/// Search movie on letterbox.
async fn search_movie(
    client: &letterboxd::Client,
    metadata: Metadata,
) -> letterboxd::Result<letterboxd::SearchResponse> {
    let request = letterboxd::SearchRequest {
        cursor: None,
        per_page: Some(1),
        input: metadata.to_string(),
        search_method: Some(letterboxd::SearchMethod::Autocomplete),
        include: Some(vec![letterboxd::SearchResultType::FilmSearchItem]),
        contribution_type: None,
    };
    client.search(&request).await
}

#[derive(Debug, Clone)]
struct Metadata {
    title: String,
    year: Option<u16>,
}

impl fmt::Display for Metadata {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.title)?;
        if let Some(year) = self.year {
            write!(f, " ({})", year)?;
        }
        Ok(())
    }
}

fn guess_metadata_with_regex(s: &str) -> Option<Metadata> {
    lazy_static! {
        static ref RE: Regex = Regex::new(TITLE_YEAR_RE).unwrap();
    }
    let caps = RE.captures(s)?;
    let title = caps.name("t")?.as_str().replace('.', " ");
    let year = caps
        .iter()
        .skip(2) // skip full capture and title group
        .find_map(|group| group?.as_str().parse().ok());
    Some(Metadata {
        title: title.trim().to_string(),
        year,
    })
}

fn guess_metadata(path: &Path) -> Option<Metadata> {
    let file_stem = path.file_stem()?.to_str()?;
    guess_metadata_with_regex(file_stem).or_else(|| {
        Some(Metadata {
            title: file_stem.to_string(),
            year: None,
        })
    })
}

/// Get film ids response of list entries request.
fn film_id_set_from_response(entries: Vec<letterboxd::ListEntry>) -> HashSet<String> {
    entries.into_iter().map(|entry| entry.film.id).collect()
}

async fn fetch_saved_films(
    list_id: &str,
    client: &letterboxd::Client,
) -> letterboxd::Result<HashSet<String>> {
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

fn load_ids_list_from_cache(path: impl AsRef<Path>) -> anyhow::Result<HashMap<String, String>> {
    let file = fs::File::open(path);
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

fn save_ids_list_to_cache(
    ids: &HashMap<String, String>,
    path: impl AsRef<Path>,
) -> anyhow::Result<()> {
    let file = fs::File::create(path)?;
    Ok(serde_json::to_writer_pretty(file, &ids)?)
}

/// Resolve movie ids from movie names by first looking in the given cache, and then, if not found,
/// by making a request through letterboxd api.
async fn resolve_film_ids(
    movies: impl Iterator<Item = (String, Metadata)>,
    film_ids_cache: &HashMap<String, String>,
    client: &letterboxd::Client,
) -> letterboxd::Result<HashMap<String, String>> {
    let film_id_requests = movies.into_iter().map(|(file_name, metadata)| async move {
        if let Some(id) = film_ids_cache.get(&file_name) {
            info!("Resolved id {} of {} from cache", id, metadata.to_string());
            Ok(Some((file_name, id.clone())))
        } else {
            let response = search_movie(&client, metadata.clone()).await?;
            let first_item = response.items.into_iter().next();
            match first_item {
                Some(letterboxd::AbstractSearchItem::FilmSearchItem { film, .. }) => {
                    info!("Resolved id {} of {}", film.id, metadata.to_string());
                    Ok(Some((file_name, film.id)))
                }
                _ => {
                    warn!("Did not find id for: {}", metadata.to_string());
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
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("debug")).init();
    dotenv::dotenv().ok();

    let cache_path = get_cache_filename().context("failed to resolve cache path")?;

    let movie_files = list_movie_files(args.directory.clone(), !args.no_recursive)
        .with_context(|| format!("failed to list files in '{}'", args.directory.display()))?;
    info!("Found {} movie files", movie_files.len());
    let movies = movie_files.into_iter().filter_map(|entry| {
        Some((
            entry.file_name().to_str()?.to_string(),
            guess_metadata(entry.path())?,
        ))
    });

    let client = new_client().await?;

    // Resolve movie ids either from cache or by requesting these
    let film_ids_cache = load_ids_list_from_cache(&cache_path)
        .with_context(|| format!("failed to read cache file at: {}", cache_path.display()))?;
    let film_ids = resolve_film_ids(movies, &film_ids_cache, &client)
        .await
        .context("failed to resolve film ids")?;

    // Fetch ids for films already on list.
    let saved_film_ids = fetch_saved_films(&args.list_id, &client)
        .await
        .context("failed to fetch ids already on the list")?;

    if let Err(err) = save_ids_list_to_cache(&film_ids, cache_path) {
        warn!("failed to save film ids to cache: {}", err);
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
