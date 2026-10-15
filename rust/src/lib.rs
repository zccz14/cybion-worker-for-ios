// The iOS C entry points (`entry`) are the only production consumers of these
// modules; host builds exist to run the unit tests.
#![cfg_attr(not(target_os = "ios"), allow(dead_code))]

mod bash;
mod computer;
mod config;
mod delivery;
mod engine;
mod entry;
mod logging;
mod pairing;
mod platform;
mod protocol;
mod resources;
mod shell;
mod state;
mod tools;
mod upgrade;
