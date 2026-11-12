//! SeeMyMac: a remote-desktop client for Macs (Apple Screen Sharing / VNC).

pub mod app;
pub mod geometry;
pub mod hp;
pub mod inhibit;
pub mod model;
pub mod rdp;
pub mod rdp_gateway;
pub mod rdp_rail;
#[cfg(target_os = "linux")]
pub mod rdp_scard;
pub mod rfb;
pub mod settings;
pub mod span;
pub mod update;
pub mod state;
pub mod vault;
pub mod panels;
pub mod view;

#[cfg(test)]
mod ui_tests;
