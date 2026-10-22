//! A name-addressed local store for published envoy bundles.
//!
//! A bundle published to a network share is copied into the store the first
//! time it is needed, then reused from local disk. The store never serves a
//! partial copy: a fill happens in a staging directory next to the store and
//! is renamed into place only when complete, so a crash at any point leaves
//! either the old state or the new one.
//!
//! Entries are addressed by `namespace:name` and the exact version string, so
//! looking one up needs no hashing and no walk of the bundle. Several envoys
//! can share one store: an advisory lock per entry makes them take turns
//! filling, while any number may run from a filled entry at once.
//!
//! ```no_run
//! use envoy_store::{EntryKey, Quiet, Store};
//! use std::path::Path;
//!
//! # fn main() -> envoy_store::Result<()> {
//! let store = Store::open("C:/Users/me/.envoy/cache")?;
//! let key = EntryKey::new("ext", "python", "3.11.9-envoy.2")?;
//! let cached = store.ensure(&key, Path::new("//server/bndl/ext/python/3.11.9-envoy.2"), &Quiet)?;
//! println!("run from {}", cached.path.display());
//! # Ok(())
//! # }
//! ```

mod error;
mod fill;
mod key;
mod lock;
mod observer;
mod state;
mod store;

pub use error::{Result, StoreError};
pub use key::EntryKey;
pub use lock::Lease;
pub use observer::{Observer, Quiet};
pub use state::{EntryState, STORE_FORMAT};
pub use store::{Ensured, EntryInfo, Outcome, PrunePolicy, PruneReport, Store};

#[cfg(test)]
mod tests;
