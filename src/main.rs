mod api;
mod app;
mod auth;
mod buffering;
mod color_state;
mod config;
mod cover;
mod download_queue;
mod keys;
mod local;
mod lyrics;
mod metadata;
mod panel;
mod player;
mod preview;
mod spectrum;
mod styling;
#[cfg(test)]
mod test_support;
mod utils;

use std::sync::OnceLock;

pub static DOWNLOAD_QUEUE: OnceLock<crate::download_queue::DownloadQueue> = OnceLock::new();

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lumitide", about = "Lumitide — Tidal CLI music player")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Search tracks, albums (-A), or artist top tracks (-a)
    Search {
        #[arg(help = "Search query, e.g. \"Netsky\" or \"Chase The Sun\"")]
        query: Option<String>,
        #[arg(short = 'n', long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=100), help = "Number of results (1-100)")]
        limit: u32,
        #[arg(short, long, help = "Search by artist and show their top tracks")]
        artist: bool,
        #[arg(
            short = 'A',
            long,
            conflicts_with = "artist",
            help = "Search albums and browse their tracks"
        )]
        album: bool,
    },
    /// Open the TUI and start an album automatically
    Play {
        #[command(subcommand)]
        target: PlayTarget,
    },
    /// Browse and play curated Tidal mixes
    Mix {
        #[arg(long, hide = true)]
        debug: bool,
    },
    /// Browse and play your Tidal playlists
    Playlist {
        #[arg(long, hide = true)]
        debug: bool,
    },
    /// Browse your library (liked tracks, saved albums, followed artists)
    Library {
        #[arg(long, hide = true)]
        debug: bool,
    },
    /// Shuffle and play local audio files (FLAC, MP3, M4A)
    Local {
        #[arg(long, hide = true)]
        debug: bool,
    },
    /// Open the config file in your default editor
    Config,
}

#[derive(Subcommand)]
enum PlayTarget {
    /// Play every track of an album; prefer --id for startup scripts
    Album {
        #[arg(required_unless_present = "id", conflicts_with = "id")]
        query: Option<String>,
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        id: Option<u64>,
        #[arg(long, requires = "query", conflicts_with = "id")]
        artist: Option<String>,
    },
}

fn main() -> Result<()> {
    // When the OS invokes us as the tidal:// URI handler, write the URL to the
    // temp file that the waiting pkce_login() is polling, then exit immediately.
    #[cfg(target_os = "windows")]
    {
        let args: Vec<String> = std::env::args().collect();
        if args.len() >= 3 && args[1] == "--auth-callback" {
            let _ = std::fs::write(auth::auth_callback_file(), args[2].trim());
            return Ok(());
        }
    }

    let _ = DOWNLOAD_QUEUE.set(crate::download_queue::DownloadQueue::new());
    let cli = Cli::parse();

    match cli.command {
        None => {
            let client = api::TidalClient::new(auth::get_session()?);
            app::run(client, app::Start::Home)
        }

        Some(Commands::Search {
            query,
            limit,
            artist,
            album,
        }) => {
            let query = query.unwrap_or_else(|| {
                Cli::command()
                    .find_subcommand_mut("search")
                    .expect("search subcommand must exist")
                    .error(
                        clap::error::ErrorKind::MissingRequiredArgument,
                        "Please provide a search query.\n\nExamples:\n  lumitide search <query>\n  lumitide search <artist> -a",
                    )
                    .exit();
            });
            let session = auth::get_session()?;
            let client = api::TidalClient::new(session);
            app::run(
                client,
                app::Start::Search {
                    query,
                    limit,
                    kind: if album {
                        app::SearchKind::Albums
                    } else if artist {
                        app::SearchKind::Artists
                    } else {
                        app::SearchKind::Tracks
                    },
                },
            )
        }

        Some(Commands::Play {
            target: PlayTarget::Album { query, id, artist },
        }) => {
            let client = api::TidalClient::new(auth::get_session()?);
            app::run(client, app::Start::Album { query, id, artist })
        }

        Some(Commands::Mix { debug }) => {
            let session = auth::get_session()?;
            let client = api::TidalClient::new(session);
            let _ = debug;
            app::run(client, app::Start::Mixes)
        }

        Some(Commands::Playlist { debug }) => {
            let session = auth::get_session()?;
            let client = api::TidalClient::new(session);
            let _ = debug;
            app::run(client, app::Start::Playlists)
        }

        Some(Commands::Library { debug }) => {
            let session = auth::get_session()?;
            let client = api::TidalClient::new(session);
            let _ = debug;
            app::run(client, app::Start::Library)
        }

        Some(Commands::Local { debug }) => local::run(debug).map(|_| ()),

        Some(Commands::Config) => config::open_editor(),
    }
}
