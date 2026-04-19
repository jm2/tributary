//! Internet Radio — Radio-Browser API integration.
//!
//! Provides access to the [Radio-Browser](https://www.radio-browser.info/)
//! community database of internet radio stations. Supports top-clicked,
//! top-voted, and geo-located ("Stations Near Me") station lists.

pub mod api;
pub mod client;
pub mod geo;

pub use api::RadioStation;
pub use client::RadioBrowserClient;
