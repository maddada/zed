//! The X11 scale factor from `GDK_SCALE` and from XSettings.
//!
//! A Wayland compositor that leaves XWayland clients unscaled (Hyprland's
//! `xwayland:force_zero_scaling`) shows them in physical pixels and expects
//! each to scale itself. Such sessions state the scale in `GDK_SCALE`, which
//! GTK and so WebKitGTK windows embedded in a GPUI window obey, and often set
//! no `Xft.dpi`. Without either, the scale factor fell back to the monitor's
//! physical DPI: 1.4167 for a 27" 4K monitor the session scales by 2, leaving
//! GPUI content smaller than the GTK content beside it. `Xft.dpi` still wins,
//! so only sessions that would have used that DPI guess change.
//!
//! Xfce's Window Scaling (Appearance > Settings) sets only the XSettings value
//! `Gdk/WindowScalingFactor`; xfsettingsd keeps `Xft.dpi` at the unscaled DPI
//! (or leaves it out). GTK multiplies the two, so an Xfce session scaled by 2
//! drew its terminals at 2x while GPUI, reading only `Xft.dpi` and a virtual
//! machine's made-up monitor size, drew at 1x (ghostex#413). A whole window
//! scale above 1 in XSettings is therefore read the way GTK reads it.

use x11rb::{
    connection::Connection,
    protocol::xproto::{AtomEnum, ConnectionExt as _},
};

/// The scale factor `GDK_SCALE` asks for, if it is set to one.
pub(crate) fn gdk_scale() -> Option<f32> {
    let scale = gdk_scale_env()?;
    log::info!("Using scale factor from GDK_SCALE: {}", scale);
    Some(scale)
}

fn gdk_scale_env() -> Option<f32> {
    parse_gdk_scale(&std::env::var("GDK_SCALE").ok()?)
}

/// GDK scales windows by whole numbers only. A value that is not a positive
/// whole number is left to the other sources rather than guessed at.
fn parse_gdk_scale(value: &str) -> Option<f32> {
    match value.trim().parse::<u32>() {
        Ok(scale) if scale > 0 => Some(scale as f32),
        _ => None,
    }
}

/// The scale GTK windows on this screen are drawn at, when the XSettings
/// manager scales windows by a whole number above 1: that window scale times
/// the font DPI GTK uses (`Gdk/UnscaledDPI`, else `Xft/DPI`) over 96.
///
/// `None` when `GDK_SCALE` is set (GTK lets it override XSettings, and the
/// existing order already handles it), when no XSettings manager runs, or
/// when it leaves windows unscaled, so those sessions keep `Xft.dpi` and the
/// other sources exactly as before. GNOME, MATE, Cinnamon and Plasma put the
/// scaled DPI in `Xft.dpi` too; they also set `Gdk/UnscaledDPI`, so the result
/// matches what `Xft.dpi` gave them.
pub(crate) fn xsettings_scale(connection: &impl Connection, screen_index: usize) -> Option<f32> {
    if gdk_scale_env().is_some() {
        return None;
    }
    let settings = read_xsettings(connection, screen_index)?;
    let scale = gtk_scale_from_xsettings(&settings)?;
    log::info!("Using scale factor from XSettings: {}", scale);
    Some(scale)
}

fn gtk_scale_from_xsettings(settings: &[(String, i32)]) -> Option<f32> {
    let int = |name: &str| {
        settings
            .iter()
            .find(|(setting, _)| setting == name)
            .map(|(_, value)| *value)
    };
    let window_scale = int("Gdk/WindowScalingFactor").filter(|scale| *scale > 1)?;
    // Both DPI settings are in 1024ths of a dot per inch. Out-of-range values
    // are ignored the way xfsettingsd clamps them.
    let dpi = int("Gdk/UnscaledDPI")
        .or_else(|| int("Xft/DPI"))
        .map(|dpi| dpi as f32 / 1024.0)
        .filter(|dpi| (50.0..=500.0).contains(dpi))
        .unwrap_or(96.0);
    Some(window_scale as f32 * dpi / 96.0)
}

/// The integer settings the screen's XSettings manager publishes, read from
/// the `_XSETTINGS_SETTINGS` property of the `_XSETTINGS_S<screen>` selection
/// owner (freedesktop XSettings specification).
fn read_xsettings(connection: &impl Connection, screen_index: usize) -> Option<Vec<(String, i32)>> {
    let selection = connection
        .intern_atom(true, format!("_XSETTINGS_S{screen_index}").as_bytes())
        .ok()?
        .reply()
        .ok()?
        .atom;
    let property = connection
        .intern_atom(true, b"_XSETTINGS_SETTINGS")
        .ok()?
        .reply()
        .ok()?
        .atom;
    if selection == x11rb::NONE || property == x11rb::NONE {
        return None;
    }
    let owner = connection
        .get_selection_owner(selection)
        .ok()?
        .reply()
        .ok()?
        .owner;
    if owner == x11rb::NONE {
        return None;
    }
    let reply = connection
        .get_property(false, owner, property, AtomEnum::ANY, 0, u32::MAX / 4)
        .ok()?
        .reply()
        .ok()?;
    if reply.format != 8 {
        return None;
    }
    parse_xsettings_ints(&reply.value)
}

/// Parses an XSettings blob, keeping the integer settings. Strings and colors
/// are skipped; a truncated blob yields `None`.
fn parse_xsettings_ints(data: &[u8]) -> Option<Vec<(String, i32)>> {
    let big_endian = match *data.first()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mut cursor = 0usize;
    let mut take = |len: usize| -> Option<&[u8]> {
        let bytes = data.get(cursor..cursor.checked_add(len)?)?;
        cursor += len;
        Some(bytes)
    };
    let u16_of = |bytes: &[u8]| {
        let bytes = [bytes[0], bytes[1]];
        if big_endian {
            u16::from_be_bytes(bytes)
        } else {
            u16::from_le_bytes(bytes)
        }
    };
    let u32_of = |bytes: &[u8]| {
        let bytes = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if big_endian {
            u32::from_be_bytes(bytes)
        } else {
            u32::from_le_bytes(bytes)
        }
    };
    let padded = |len: usize| len.div_ceil(4) * 4;

    take(4)?; // byte order and padding
    take(4)?; // serial
    let count = u32_of(take(4)?);
    let mut settings = Vec::new();
    for _ in 0..count {
        let header = take(4)?;
        let kind = header[0];
        let name_len = u16_of(&header[2..4]) as usize;
        let name = take(padded(name_len))?.get(..name_len)?;
        let name = String::from_utf8_lossy(name).into_owned();
        take(4)?; // last-change serial
        match kind {
            0 => settings.push((name, u32_of(take(4)?) as i32)),
            1 => {
                let len = u32_of(take(4)?) as usize;
                take(padded(len))?;
            }
            2 => {
                take(8)?;
            }
            _ => return None,
        }
    }
    Some(settings)
}

#[cfg(test)]
mod tests {
    use super::parse_gdk_scale;

    #[test]
    fn a_whole_gdk_scale_is_the_scale_factor() {
        assert_eq!(parse_gdk_scale("2"), Some(2.0));
        assert_eq!(parse_gdk_scale("1"), Some(1.0));
    }

    #[test]
    fn a_gdk_scale_that_is_not_a_positive_whole_number_is_ignored() {
        assert_eq!(parse_gdk_scale(""), None);
        assert_eq!(parse_gdk_scale("0"), None);
        assert_eq!(parse_gdk_scale("1.5"), None);
        assert_eq!(parse_gdk_scale("-2"), None);
        assert_eq!(parse_gdk_scale("auto"), None);
    }
}
