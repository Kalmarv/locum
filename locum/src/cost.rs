//! What locum's own work cost since the last report, as one log line: on the game's threads (resources made,
//! updates, a capture's map shadows, recorded events and contents copies) and on the writer's (blobs written, and what happened to
//! blobs it could not take at once).

use locum_capture::writer;

use crate::d3d11::capture::{COPIED, RECORDED, ops::SHADOWED};
use crate::d3d11::registry::{REGISTERED, UPDATES};

/// The counts since the last call, which starts them again from zero.
pub fn line() -> String {
    format!(
        "locum on the game's threads: resources made {}, updates {}, maps shadowed {}, events recorded {}, contents copied {}; writer: blobs written {}, already taken {}, dropped {}, waited for {}",
        REGISTERED.take(),
        UPDATES.take(),
        SHADOWED.take(),
        RECORDED.take(),
        COPIED.take(),
        writer::WRITTEN.take(),
        writer::KNOWN.take(),
        writer::DROPPED.take(),
        writer::WAITED.take()
    )
}
