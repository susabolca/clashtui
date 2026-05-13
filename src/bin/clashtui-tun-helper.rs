#![allow(dead_code)]

#[path = "../config.rs"]
mod config;
#[path = "../platform/mod.rs"]
mod platform;
#[path = "../privilege/mod.rs"]
mod privilege;

use anyhow::Result;

fn main() -> Result<()> {
    privilege::tun_helper_run()
}
