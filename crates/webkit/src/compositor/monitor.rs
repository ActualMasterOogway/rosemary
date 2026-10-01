//! The host's DRM connectors, read to give each output the physical size of
//! the display it stands for.
//!
//! WebKitGTK paces `requestAnimationFrame` with a vblank monitor that finds
//! its display's CRTC by comparing the `GdkMonitor`'s physical size with each
//! connected connector's. An output with the real size lets it wait on the
//! real vblank at the real rate; one it cannot match leaves it on a timer
//! fixed at 60 Hz.

use std::os::fd::{AsFd, BorrowedFd};

use smithay::reexports::drm::control::{Device as ControlDevice, connector};

use super::Monitor;

/// A connected connector: its name, as `DP-1`, its size in millimetres,
/// and the mode its CRTC is showing, as pixels and millihertz.
#[derive(Clone, Debug, PartialEq)]
pub struct Connector {
    pub name: String,
    pub size: (u32, u32),
    pub mode: Option<(u32, u32, u32)>,
}

struct Card(std::fs::File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl smithay::reexports::drm::Device for Card {}
impl ControlDevice for Card {}

/// Every connected connector on every card; reading them needs no DRM
/// master, and the kernel does not reprobe for a client that is not one.
pub fn connectors() -> Vec<Connector> {
    let Ok(entries) = std::fs::read_dir("/dev/dri") else {
        return Vec::new();
    };
    let mut cards: Vec<_> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("card"))
        })
        .collect();
    cards.sort();
    let mut out = Vec::new();
    for path in cards {
        let Ok(file) = std::fs::File::open(&path) else {
            continue;
        };
        let card = Card(file);
        let Ok(resources) = card.resource_handles() else {
            continue;
        };
        for &handle in resources.connectors() {
            let Ok(info) = card.get_connector(handle, false) else {
                continue;
            };
            if info.state() != connector::State::Connected || info.modes().is_empty() {
                continue;
            }
            let mode = info
                .current_encoder()
                .and_then(|encoder| card.get_encoder(encoder).ok()?.crtc())
                .and_then(|crtc| card.get_crtc(crtc).ok()?.mode())
                .map(|mode| {
                    let (width, height) = mode.size();
                    (u32::from(width), u32::from(height), millihertz(&mode))
                });
            out.push(Connector {
                name: info.to_string(),
                size: info.size().unwrap_or((0, 0)),
                mode,
            });
        }
    }
    out
}

fn millihertz(mode: &smithay::reexports::drm::control::Mode) -> u32 {
    let (_, _, htotal) = mode.hsync();
    let (_, _, vtotal) = mode.vsync();
    let pixels = u64::from(htotal) * u64::from(vtotal);
    if pixels == 0 {
        return mode.vrefresh() * 1000;
    }
    // The clock is in kilohertz.
    (u64::from(mode.clock()) * 1_000_000 / pixels) as u32
}

/// The physical size of the connector `monitor` is: the one with its name,
/// or else the only one showing its mode.
pub fn physical_size(connectors: &[Connector], monitor: &Monitor) -> Option<(u32, u32)> {
    if let Some(name) = &monitor.name
        && let Some(found) = connectors.iter().find(|c| c.name == *name)
    {
        return Some(found.size);
    }
    let showing = |c: &&Connector| {
        c.mode.is_some_and(|(width, height, refresh)| {
            (width as i32, height as i32) == (monitor.width, monitor.height)
                && refresh.abs_diff(monitor.refresh) <= 1000
        })
    };
    let mut matching = connectors.iter().filter(showing);
    let found = matching.next()?;
    matching.next().is_none().then_some(found.size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor(name: Option<&str>, refresh: u32) -> Monitor {
        Monitor {
            name: name.map(str::to_string),
            width: 2560,
            height: 1440,
            refresh,
        }
    }

    fn connector(name: &str, size: (u32, u32), refresh: u32) -> Connector {
        Connector {
            name: name.to_string(),
            size,
            mode: Some((2560, 1440, refresh)),
        }
    }

    #[test]
    fn a_display_is_found_by_its_connector_name() {
        let all = [
            connector("DP-1", (600, 340), 165_000),
            connector("DP-2", (700, 390), 165_000),
        ];
        assert_eq!(
            physical_size(&all, &monitor(Some("DP-2"), 60_000)),
            Some((700, 390))
        );
    }

    #[test]
    fn an_unnamed_display_is_found_by_its_only_matching_mode() {
        let all = [
            connector("DP-1", (600, 340), 165_000),
            connector("HDMI-A-1", (520, 290), 59_951),
        ];
        assert_eq!(
            physical_size(&all, &monitor(Some("Dell"), 60_000)),
            Some((520, 290))
        );
    }

    #[test]
    fn two_displays_showing_the_same_mode_are_not_guessed_between() {
        let all = [
            connector("DP-1", (600, 340), 144_000),
            connector("DP-2", (700, 390), 144_000),
        ];
        assert_eq!(physical_size(&all, &monitor(None, 144_000)), None);
    }
}
