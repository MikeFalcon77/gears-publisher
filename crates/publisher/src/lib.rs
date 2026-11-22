//! The publisher gear: releases gears-rust and runs its PR CI from a web UI.
mod api;
pub mod config;
pub mod domain;
mod gear;
pub mod infra;

pub use gear::PublisherGear;
