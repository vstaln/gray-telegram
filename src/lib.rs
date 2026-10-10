pub mod attachments;
pub mod budget;
pub mod capabilities;
pub mod cli;
pub mod commands;
pub mod config;
pub mod doctor;
pub mod durable;
pub mod gateway;
pub mod lifecycle;
pub mod media_tags;
pub mod pairing;
pub mod policy;
pub mod ratelimit;
pub mod runner;
pub mod service;
pub mod session;
pub mod setup;
pub mod sidecar;
pub mod telegram_check;
pub mod text;
pub mod transport;

#[cfg(test)]
mod e2e_mock;
