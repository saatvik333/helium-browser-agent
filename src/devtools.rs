//! Developer tools for agents: console, errors, the network log and interception, performance
//! tools, and the browser's own developer tools window.

mod coverage;
mod har;
mod logs;
mod net;
mod panel;
mod perf;
mod vitals;

pub use logs::{console, errors};
pub use net::net;
pub use panel::open_devtools;
pub use perf::{State, on_event, perf};
