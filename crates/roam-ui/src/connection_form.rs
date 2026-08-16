use std::collections::BTreeMap;

use gpui::{App, AppContext, Entity, Window};
use gpui_component::input::InputState;
use roam_core::{Profile, ProfileId, Result, profile};

use crate::placeholders;

/// What the form produced: a profile to save, plus any credentials to hand to
/// the secret store.
pub struct Draft {
    pub profile: Profile,
    /// Empty when editing and the credential fields were left blank, which
    /// means "keep whatever is already stored".
    pub credentials: BTreeMap<String, String>,
}

/// Input state for the new/edit connection dialog.
pub struct ConnectionForm {
    /// `Some` when editing an existing profile; its id is preserved so the
    /// stored credentials stay reachable.
    pub editing: Option<ProfileId>,
    pub name: Entity<InputState>,
    pub uri: Entity<InputState>,
    pub options: Entity<InputState>,
    pub credentials: Entity<InputState>,
}

impl ConnectionForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            editing: None,
            name: cx
                .new(|cx| InputState::new(window, cx).placeholder(placeholders::CONNECTION_NAME)),
            uri: cx.new(|cx| InputState::new(window, cx).placeholder(placeholders::CONNECTION_URI)),
            options: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(4)
                    .placeholder(placeholders::CONNECTION_OPTIONS)
            }),
            credentials: cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .rows(3)
                    .placeholder(placeholders::CONNECTION_CREDENTIALS)
            }),
        }
    }

    pub fn editing(profile: &Profile, window: &mut Window, cx: &mut App) -> Self {
        let form = Self::new(window, cx);

        form.name.update(cx, |state, cx| {
            state.set_value(profile.name.clone(), window, cx)
        });
        form.uri.update(cx, |state, cx| {
            state.set_value(profile.uri.clone(), window, cx)
        });

        let options = profile
            .options
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect::<Vec<_>>()
            .join("\n");
        form.options
            .update(cx, |state, cx| state.set_value(options, window, cx));

        // Credentials are deliberately not read back out of the keychain to
        // populate this field. Leaving it blank keeps the stored values; typing
        // in it replaces them.

        Self {
            editing: Some(profile.id.clone()),
            ..form
        }
    }

    /// Validate the inputs and assemble a [`Draft`].
    ///
    /// `existing` is the profile being edited, if any — its `secrets` list is
    /// carried forward so that re-entering only one of two credentials does not
    /// drop the other.
    pub fn build(
        &self,
        existing: Option<&Profile>,
        taken: &[ProfileId],
        cx: &App,
    ) -> Result<Draft> {
        let name = self.name.read(cx).value().trim().to_string();
        let uri = self.uri.read(cx).value().trim().to_string();

        if name.is_empty() {
            return Err(roam_core::Error::Config("请填写连接名称".into()));
        }

        let options = profile::parse_kv_lines(&self.options.read(cx).value())?;
        let credentials = profile::parse_kv_lines(&self.credentials.read(cx).value())?;

        let id = match &self.editing {
            Some(id) => id.clone(),
            None => profile::unique_id(&name, taken),
        };

        let mut secrets: Vec<String> = existing.map(|p| p.secrets.clone()).unwrap_or_default();
        for key in credentials.keys() {
            if !secrets.contains(key) {
                secrets.push(key.clone());
            }
        }
        secrets.sort();

        let profile = Profile {
            id,
            name,
            uri,
            options,
            secrets,
        };

        // Catches a credential typed into the options box, among other things.
        profile.validate()?;

        Ok(Draft {
            profile,
            credentials,
        })
    }
}
