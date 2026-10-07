//! The asset source GPUI loads the widget kit's icons from.

use std::borrow::Cow;

use gpui::{AssetSource, Result, SharedString};

const ICONS: &[(&str, &[u8])] = &[
    ("icons/alert.svg", include_bytes!("../../assets/icons/alert.svg")),
    ("icons/check.svg", include_bytes!("../../assets/icons/check.svg")),
    ("icons/check-circle.svg", include_bytes!("../../assets/icons/check-circle.svg")),
    ("icons/chevron-down.svg", include_bytes!("../../assets/icons/chevron-down.svg")),
    ("icons/chevron-right.svg", include_bytes!("../../assets/icons/chevron-right.svg")),
    ("icons/close.svg", include_bytes!("../../assets/icons/close.svg")),
    ("icons/computer.svg", include_bytes!("../../assets/icons/computer.svg")),
    ("icons/copy.svg", include_bytes!("../../assets/icons/copy.svg")),
    ("icons/download.svg", include_bytes!("../../assets/icons/download.svg")),
    ("icons/refresh.svg", include_bytes!("../../assets/icons/refresh.svg")),
    ("icons/trash.svg", include_bytes!("../../assets/icons/trash.svg")),
    ("icons/eye.svg", include_bytes!("../../assets/icons/eye.svg")),
    ("icons/eye-off.svg", include_bytes!("../../assets/icons/eye-off.svg")),
    ("icons/key.svg", include_bytes!("../../assets/icons/key.svg")),
];

pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok(ICONS.iter().find(|(name, _)| *name == path).map(|(_, bytes)| Cow::Borrowed(*bytes)))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(ICONS.iter().filter(|(name, _)| name.starts_with(path)).map(|(name, _)| SharedString::from(*name)).collect())
    }
}
