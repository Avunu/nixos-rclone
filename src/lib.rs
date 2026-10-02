//! Supervisor daemon for the nixos-rclone NixOS module.
//!
//! The NixOS module serialises its option tree to JSON (see [`config`]); every
//! subcommand of the binary reads that file and nothing else.

pub mod bisync;
pub mod config;
pub mod ctl;
pub mod filter;
pub mod identity;
pub mod listing;
pub mod markdown;
pub mod mounts;
pub mod pair;
pub mod push;
pub mod rc;
pub mod timespan;
pub mod watch;
