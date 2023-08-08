use std::fmt::{self, Display, Formatter};

use serde::{Deserialize, Serialize};

use casper_types::{BlockSignatures, VersionedBlock};

/// A block and signatures for that block.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBlock {
    /// Block.
    pub(crate) block: VersionedBlock,
    // The signatures of the block.
    pub(crate) block_signatures: BlockSignatures,
}

impl Display for SignedBlock {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "block #{}, {}, with {} block signatures",
            self.block.height(),
            self.block.hash(),
            self.block_signatures.len()
        )
    }
}
