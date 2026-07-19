//! Internet Radio — Radio-Browser API integration.
//!
//! Provides access to the [Radio-Browser](https://www.radio-browser.info/)
//! community database of internet radio stations. Supports top-clicked,
//! top-voted, and geo-located ("Stations Near Me") station lists.

pub mod adapter;
mod api;
mod client;
mod geo;
