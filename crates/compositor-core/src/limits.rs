//! The size and memory ceilings a document is held to, in one place.

/// The document limits, in one place (Swift `DocumentLimits`).
///
/// Two separate ideas: how large a *single* surface may be, and how much raster a *whole document* may
/// hold across all of its layers. Both pixel ceilings stay below `MAX_SIDE * MAX_SIDE`, so a square at
/// `MAX_SIDE` is still rejected as oversized.
pub mod limits {
    use crate::geom::CGFloat;

    /// Longest side, in pixels, of any canvas, layer, mask or generated surface.
    pub const MAX_SIDE: usize = 30_000;

    /// `MAX_SIDE` for the paths that measure in `CGFloat`.
    pub const MAX_SIDE_EXTENT: CGFloat = MAX_SIDE as CGFloat;

    /// Largest single surface: a canvas, an export, a filter target, an adjustment or mask render.
    /// At RGBA8 one allocation is at most 800 MB, and a filter holds a few of them at once.
    pub const MAX_SURFACE_PIXELS: usize = 200_000_000;

    /// `MAX_SURFACE_PIXELS` for the paths that measure in `CGFloat`.
    pub const MAX_SURFACE_EXTENT: CGFloat = MAX_SURFACE_PIXELS as CGFloat;

    /// Total imported raster one document may hold, summed across every layer and mask.
    ///
    /// Scaled to the machine: a quarter of its memory at 4 bytes a pixel (about 537 MP on 8 GB), never
    /// less than one surface and never more than 800 MP (3.2 GB of layers), which a 16 GB machine
    /// already reaches.
    pub fn document_pixel_budget() -> usize {
        800_000_000usize.min(MAX_SURFACE_PIXELS.max(physical_memory() / 16))
    }

    /// The two ceilings as megapixels, for the messages that quote them back to the reader.
    pub fn max_surface_megapixels() -> usize {
        MAX_SURFACE_PIXELS / 1_000_000
    }

    pub fn document_budget_megapixels() -> usize {
        document_pixel_budget() / 1_000_000
    }

    /// The host's physical memory in bytes.
    pub fn physical_memory() -> usize {
        imp::physical_memory()
    }

    #[cfg(target_os = "windows")]
    mod imp {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

        pub fn physical_memory() -> usize {
            // SAFETY: the structure is fully initialized before the call, as the API requires.
            unsafe {
                let mut status: MEMORYSTATUSEX = std::mem::zeroed();
                status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
                if GlobalMemoryStatusEx(&mut status) != 0 {
                    status.ullTotalPhys as usize
                } else {
                    8 * 1024 * 1024 * 1024
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    mod imp {
        pub fn physical_memory() -> usize {
            std::fs::read_to_string("/proc/meminfo")
                .ok()
                .and_then(|text| {
                    text.lines()
                        .find(|line| line.starts_with("MemTotal:"))
                        .and_then(|line| line.split_whitespace().nth(1).and_then(|value| value.parse::<usize>().ok()))
                        .map(|kilobytes| kilobytes * 1024)
                })
                .unwrap_or(8 * 1024 * 1024 * 1024)
        }
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    mod imp {
        pub fn physical_memory() -> usize {
            std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
                .ok()
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .and_then(|text| text.trim().parse::<usize>().ok())
                .unwrap_or(8 * 1024 * 1024 * 1024)
        }
    }
}
