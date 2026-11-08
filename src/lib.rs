//! Arr: movie/TV API server.
//!
//! Sonarr/Radarr data enters only through the offline snapshot importer. Runtime integrations
//! are Prowlarr (search, downloads, Torznab RSS), TVmaze/TMDB metadata, and, only with
//! `[execution] enabled`, qBittorrent, the configured library roots and Plex. Shadow mode
//! (the default) makes no writes outside its own private state.

pub mod api;
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
pub mod library;
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
pub mod qbit;
pub mod quality;
pub mod release;
pub mod report;
pub mod rss;
pub mod scan;
pub mod search;
pub mod settings;
pub mod setup;
pub mod store;
pub mod target;
pub mod title;
pub mod tmdbkey;
pub mod torrent;
pub mod torznab;
pub mod tpb;
pub mod upstream;
pub mod wizard;
pub mod worker;
