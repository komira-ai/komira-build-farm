//! A node's GPU count: whole GPUs, each booked by one lease at a time.
//!
//! - Linux: the PCI functions the kernel lists under `/sys/bus/pci/devices`, each read
//!   as the text of its `class` and `vendor` files. A function counts as one GPU when
//!   it is a VGA-compatible or 3D controller (class `0x0300xx` or `0x0302xx`) made by
//!   NVIDIA (`0x10de`) or AMD (`0x1002`). Other display functions do not count: a
//!   server's management controller exposes a VGA function, and integrated graphics
//!   is not a device to hand to an action.
//! - macOS: an Apple silicon Mac has one GPU, shared by the whole host.

use crate::cpu::ParseError;
use crate::macos;

/// PCI vendor ids whose display functions count as GPUs: NVIDIA and AMD.
const GPU_VENDORS: [u32; 2] = [0x10de, 0x1002];

/// PCI base class and subclass of a GPU: VGA-compatible and 3D controller.
const GPU_CLASSES: [u32; 2] = [0x0300, 0x0302];

/// One PCI function as sysfs shows it: the text of its `class` and `vendor` files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFunction<'a> {
    /// The `class` file: `0x` and six hex digits (base class, subclass, interface).
    pub class: &'a str,
    /// The `vendor` file: `0x` and four hex digits.
    pub vendor: &'a str,
}

/// The number of GPUs among a Linux node's PCI functions; see the module docs.
///
/// # Errors
/// A `class` or `vendor` text is not `0x` and hex digits of its width (a trailing
/// newline allowed, as sysfs writes it).
pub fn gpus_from_linux_pci<'a, I>(functions: I) -> Result<u64, ParseError>
where
    I: IntoIterator<Item = PciFunction<'a>>,
{
    let mut gpus = 0;
    for f in functions {
        let class = hex(f.class, 6)? >> 8;
        let vendor = hex(f.vendor, 4)?;
        if GPU_CLASSES.contains(&class) && GPU_VENDORS.contains(&vendor) {
            gpus += 1;
        }
    }
    Ok(gpus)
}

/// The number of GPUs of a Mac from its `sysctl hw.optional` text: one on Apple silicon.
///
/// # Errors
/// The text does not parse, or is not from an Apple silicon Mac; see
/// [`crate::CpuCaps::from_macos_sysctl`].
pub fn gpus_from_macos_sysctl(text: &str) -> Result<u64, ParseError> {
    macos::features(text).map(|_| 1)
}

/// `text` as `0x` and exactly `digits` hex digits, one trailing newline allowed.
fn hex(text: &str, digits: usize) -> Result<u32, ParseError> {
    let bad = || ParseError::PciValue(text.to_owned());
    let value = text.strip_suffix('\n').unwrap_or(text);
    let value = value.strip_prefix("0x").ok_or_else(bad)?;
    if value.len() != digits || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(bad());
    }
    u32::from_str_radix(value, 16).map_err(|_| bad())
}
