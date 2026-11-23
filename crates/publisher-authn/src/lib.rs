//! `AuthN` resolver plugin for the publisher: verifies Firebase ID tokens and admits
//! **every** verified Firebase user.
//!
//! Signing in is a button anyone with a Google or GitHub account can press, so a
//! verified token proves who somebody is and nothing about what they may do. That
//! decision is the publisher gear's (`publisher::domain::access`): config admins, users
//! an admin approved, and everyone else, who may only ask for access.
//!
//! Derived from `cf-gears-firebase-authn-plugin` 0.2.3 (Copyright Constructor Fabric,
//! Apache-2.0). Changes: no identity allowlist and no verified-email requirement; one
//! configured tenant and scope set for every subject; its own GTS suffix.
pub mod config;
pub mod firebase;
mod gear;
pub use gear::PublisherAuthnPlugin;
