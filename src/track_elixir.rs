//! The Elixir track. **Not live.** Registered, unbuilt.
//!
//! [`TRACK`] carries `scan: None`, which is the whole of "disabled": the
//! driver runs nothing for it, and [`crate::registry::Track::owns_extension`]
//! answers `false` for every extension [`Lang::Elixir`] claims, so no scan
//! reads `.ex` and `.exs`. Bringing Elixir up is `scan: None` becoming
//! `scan: Some(...)` **in this file** and nothing in a shared one — see
//! [`crate::registry`] for why that rule exists, and [`crate::track_go`] for
//! the shape a live track takes.

use crate::model::Lang;
use crate::registry::Track;

/// Elixir's registration. Not live, so the track owns no file and contributes
/// neither a read nor a report line.
pub const TRACK: Track = Track {
    name: "elixir",
    langs: &[Lang::Elixir],
    scan: None,
};
