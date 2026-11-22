//! Calls: the rules a Monitor applies to its Viewers, and (with the `media`
//! feature) the WebRTC peers that carry the picture and sound.

#[cfg(feature = "media")]
pub mod media;
pub mod policy;
