//! Shared connection identities for the sidebar and connection picker.
//!
//! Product logos use `img`: GPUI's monochrome SVG icon renderer discards their
//! colours. Protocols use neutral Lucide symbols rather than a vendor's logo.

use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::{AnyElement, IntoElement, ObjectFit, Styled, StyledImage, img, px};

pub(crate) enum ServiceIcon {
    Logo(&'static str),
    Symbol(IconName),
}

impl ServiceIcon {
    pub(crate) fn for_scheme(scheme: &str) -> Self {
        match scheme {
            "s3" => Self::Logo("services/amazon-s3.svg"),
            "gcs" => Self::Logo("services/google-cloud-storage.svg"),
            "azblob" => Self::Logo("services/azure-storage.svg"),
            "sharepoint" => Self::Logo("services/sharepoint.svg"),
            "webdav" => Self::Logo("services/webdav.jpg"),
            "fs" => Self::Symbol(IconName::HardDrive),
            "sftp" => Self::Symbol(IconName::FolderLock),
            "nfs" => Self::Symbol(IconName::Network),
            _ => Self::Symbol(IconName::Globe),
        }
    }

    pub(crate) fn render(self) -> AnyElement {
        match self {
            Self::Logo(path) => img(path)
                .size(px(20.))
                .flex_none()
                .object_fit(ObjectFit::Contain)
                .into_any_element(),
            Self::Symbol(icon) => Icon::new(icon).size(px(20.)).flex_none().into_any_element(),
        }
    }
}
