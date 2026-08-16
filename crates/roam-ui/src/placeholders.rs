//! Every input placeholder in the crate, in one place.
//!
//! # They must all be single-line
//!
//! gpui-component 0.5.1 lays out an *empty* input's placeholder by splitting it
//! on newlines while passing the font runs built for the **whole** string to
//! each line (`input/element.rs`, the `state.text.len() == 0` branch). The run
//! length then overruns the single line's byte length and gpui's mac text system
//! aborts the process:
//!
//! ```text
//! panicked at gpui/src/platform/mac/text_system.rs:448:
//! end byte index 46 is out of bounds for string of length 23
//! fatal runtime error: failed to initiate panic, error 3, aborting
//! ```
//!
//! Multi-line *values* are unaffected — those take the text_wrapper path, which
//! computes per-line runs correctly. Only placeholders are.
//!
//! Collecting them here means the invariant is enforced by one test instead of
//! relying on whoever adds the next input remembering. Multi-line guidance
//! belongs in a field's hint text, not its placeholder.

pub const CONNECTION_NAME: &str = "Prod S3";
pub const CONNECTION_URI: &str = "s3://bucket/prefix";
pub const CONNECTION_OPTIONS: &str = "region = ap-northeast-1";
pub const CONNECTION_CREDENTIALS: &str = "access_key_id = AKIA…";
pub const NEW_FOLDER: &str = "新文件夹";
pub const RENAME: &str = "新名称";
pub const FILTER: &str = "过滤当前目录";

/// Used by the test below. Add every new placeholder here.
pub const ALL: &[&str] = &[
    CONNECTION_NAME,
    CONNECTION_URI,
    CONNECTION_OPTIONS,
    CONNECTION_CREDENTIALS,
    NEW_FOLDER,
    RENAME,
    FILTER,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_single_line() {
        // A newline here aborts the process the moment the input renders — see
        // the module docs. Cheap to assert; brutal to debug.
        for placeholder in ALL {
            assert!(
                !placeholder.contains('\n') && !placeholder.contains('\r'),
                "placeholder {placeholder:?} spans multiple lines"
            );
        }
    }

    #[test]
    fn placeholders_are_not_blank() {
        for placeholder in ALL {
            assert!(!placeholder.trim().is_empty());
        }
    }

    #[test]
    fn every_placeholder_constant_is_registered() {
        // Guards against adding a constant above but forgetting to list it in
        // ALL, which would silently exempt it from the single-line check.
        let source = include_str!("placeholders.rs");
        let declared = source
            .lines()
            .filter(|line| line.starts_with("pub const ") && !line.contains("ALL"))
            .count();

        assert_eq!(
            declared,
            ALL.len(),
            "a placeholder constant is missing from ALL"
        );
    }
}
