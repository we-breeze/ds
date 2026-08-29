mod ephemeral;
mod malloc;

pub use ephemeral::{
    EPHEMERAL_BYTES_INLINE_CAPACITY, EphemeralBytes, EphemeralBytesArena, EphemeralBytesMut,
};
pub use malloc::*;
