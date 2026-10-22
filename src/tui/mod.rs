//! The terminal UI Dang reads and posts from.
//!
//! * [`state`]: the view model, the messages that change it and the commands
//!   it asks for ([`state::App::update`], pure and synchronous);
//! * [`keys`]: what each key does;
//! * [`convert`]: server answers turned into view structs;
//! * [`view`] and [`widgets`]: the render, which matches the approved mockup
//!   (`docs/design/tui-mockup.html`);
//! * [`live`]: the event loop that connects them to the terminal and the
//!   node: input, the live feed with reconnects, ticks, and commands run on
//!   tokio tasks.

pub mod convert;
pub mod cursor;
pub mod keys;
pub mod live;
pub mod mention;
pub mod state;
pub mod theme;
pub mod view;
pub mod widgets;
pub mod wrap;

pub use live::{drive, run, TuiConfig};
