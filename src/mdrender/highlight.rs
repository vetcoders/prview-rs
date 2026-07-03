//! Lazily-built, shared syntect highlighters.
//!
//! Building a [`SyntectAdapter`] loads syntect's default syntax and theme sets,
//! which is expensive. We build one adapter per theme name on first use and
//! cache it behind a `Mutex<HashMap<..>>`, so repeated [`render`] calls reuse
//! the same loaded syntax set instead of re-parsing it every time.
//!
//! [`render`]: crate::mdrender::render

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use comrak::plugins::syntect::{SyntectAdapter, SyntectAdapterBuilder};

/// Fallback theme when the requested name is not a known syntect default.
pub(crate) const DEFAULT_CODE_THEME: &str = "base16-ocean.dark";

/// Names shipped by `syntect::highlighting::ThemeSet::load_defaults()`.
///
/// Requesting a highlight theme outside this set would panic inside the adapter
/// (it indexes the theme map), so we validate against this list and fall back.
/// Stable across syntect 5.x.
const KNOWN_THEMES: &[&str] = &[
    "base16-ocean.dark",
    "base16-eighties.dark",
    "base16-mocha.dark",
    "base16-ocean.light",
    "InspiredGitHub",
    "Solarized (dark)",
    "Solarized (light)",
];

static ADAPTERS: OnceLock<Mutex<HashMap<String, Arc<SyntectAdapter>>>> = OnceLock::new();

/// Return a shared highlighter for `theme_name`, building and caching it once.
///
/// An unknown theme name resolves to [`DEFAULT_CODE_THEME`].
pub(crate) fn adapter_for(theme_name: &str) -> Arc<SyntectAdapter> {
    let resolved = if KNOWN_THEMES.contains(&theme_name) {
        theme_name
    } else {
        DEFAULT_CODE_THEME
    };

    let cache = ADAPTERS.get_or_init(|| Mutex::new(HashMap::new()));

    // Fast path: adapter already built — grab it and release the lock.
    {
        let map = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(adapter) = map.get(resolved) {
            return Arc::clone(adapter);
        }
    }

    // Build the adapter *outside* the lock. Loading syntect's syntax/theme sets
    // is expensive, and holding the Mutex across it serializes every concurrent
    // render. A racing thread may build the same theme meanwhile; that is cheap
    // insurance against a slow critical section.
    let adapter = Arc::new(SyntectAdapterBuilder::new().theme(resolved).build());

    // Re-acquire the lock and commit. `or_insert` keeps the first writer's
    // adapter, so the cache stays exactly one adapter per theme.
    let mut map = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(map.entry(resolved.to_string()).or_insert(adapter))
}
