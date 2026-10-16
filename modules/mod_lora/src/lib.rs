//! LoRa module for the esp-xip-modules host: SX127x and SX126x drivers (LoRa and FSK), packet reception and
//! transmission, and the board bring-up (power, reset, SPI with the BUSY line), as an event-driven state machine
//! ([`machine::Radio`]): the host calls it for requests, for the radio's interrupt line and for its own alarm, and
//! between calls nothing runs.
//!
//! Everything LoRa-specific lives here. The drivers use only the [`hal::Hal`] trait; [`xip`] implements it
//! over the esp-xip-modules host table and exports the handler, so the same code runs as a flash module
//! (`mod_lora.dmod`, bin `mod_lora` with feature `flat-image`) or linked into an application. The host tests run
//! the machine against a scripted fake.
#![no_std]

#[cfg(test)]
extern crate std;

pub mod command;
pub mod config;
pub mod xip;
pub mod frames;
pub mod fsk;
pub mod hal;
pub mod machine;
pub mod sx126x;
pub mod sx127x;

pub use config::LoraConfig;
pub use hal::Hal;

#[cfg(test)]
mod tests;
