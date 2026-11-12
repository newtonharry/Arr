//! Arr: movie/TV API server.
//!
//! Runtime integrations are native indexers (or Prowlarr: search, downloads, Torznab RSS),
//! TVmaze/TMDB metadata, Plex (library bootstrap and refreshes), and, only with
//! `[execution] enabled`, the embedded BitTorrent engine (or qBittorrent) and the configured
//! library roots. Shadow mode (the default) makes no writes outside its own private state.
#![recursion_limit = "256"]

pub mod api;
pub mod art;
pub mod bootstrap;
pub mod catalog;
pub mod client;
pub mod config;
pub mod decision;
pub mod downloads;
pub mod embedded;
pub mod error;
pub mod fixture;
pub mod fsutil;
pub mod identity;
pub mod ids;
pub mod import;
pub mod indexer;
pub mod ingest;
pub mod knaben;
pub mod library;
pub mod lifecycle;
pub mod logbuf;
pub mod mcp;
pub mod metadata;
pub mod oauth;
pub mod openapi;
pub mod paths;
pub mod plan;
pub mod plex;
pub mod plexinstall;
pub mod plexsignin;
pub mod probe;
pub mod prowlarr;
pub mod push;
pub mod qbit;
pub mod quality;
pub mod release;
pub mod report;
pub mod rss;
pub mod scan;
pub mod search;
pub mod selfupdate;
pub mod set;
pub mod settings;
pub mod setup;
pub mod store;
pub mod target;
pub mod title;
pub mod tmdbkey;
pub mod torrent;
pub mod torznab;
pub mod tpb;
pub mod trash;
pub mod upstream;
pub mod webapp;
pub mod wizard;
pub mod worker;
