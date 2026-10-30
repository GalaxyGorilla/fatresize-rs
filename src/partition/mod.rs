//! Strict MBR and GPT partition-table views.

pub mod gpt;
pub mod mbr;

pub use gpt::{Gpt, GptHeader, GptPartition};
pub use mbr::{Mbr, MbrPartition};
