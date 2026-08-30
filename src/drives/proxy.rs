//! Hand-written zbus proxies for the UDisks2 interfaces this app calls.
//!
//! Only the methods actually used are declared: properties are read in bulk
//! from `GetManagedObjects` instead, which avoids a round-trip per property per
//! device when the sidebar refreshes.

use std::collections::HashMap;

use zbus::zvariant::Value;

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Filesystem",
    default_service = "org.freedesktop.UDisks2"
)]
pub trait Filesystem {
    /// Returns the resulting mount point.
    fn mount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<String>;

    fn unmount(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}

#[zbus::proxy(
    interface = "org.freedesktop.UDisks2.Drive",
    default_service = "org.freedesktop.UDisks2"
)]
pub trait Drive {
    fn eject(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;

    /// Cuts power to the drive so it can be physically removed.
    fn power_off(&self, options: HashMap<&str, Value<'_>>) -> zbus::Result<()>;
}
