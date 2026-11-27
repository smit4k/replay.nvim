//! Offline Neovim UI capture, RGBA rasterization, and FFmpeg export.

pub mod encoder;
pub mod headless_nvim;
pub mod rasterizer;

use std::process::Child;

fn reap_child(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
}
