//! The new/edit connection dialog.
//!
//! Every input here is generated from [`roam_core::service`], so the form asks
//! for exactly what the chosen backend needs and nothing else. It used to be two
//! free-text boxes of `key = value` lines, which meant a typo in an option name
//! was only discovered at connect time — and the URI had to be hand-written.
//!
//! This is a view rather than a plain struct because picking a different service
//! rebuilds the field list, and a rebuild has to re-render.

use std::collections::BTreeMap;

use gpui::{
    AnyElement, App, AppContext, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Pixels, Render, SharedString, Styled, Window, div, prelude::FluentBuilder, px,
};
use gpui_component::button::{Button, ButtonVariants};
use gpui_component::input::{Input, InputState};
use gpui_component::scroll::ScrollableElement;
use gpui_component::switch::Switch;
use gpui_component::{ActiveTheme, Sizable, h_flex, v_flex};
use roam_core::service::{self, Field, FieldKind, Service};
use roam_core::{Profile, ProfileId, Result, profile};

use crate::placeholders;

/// One rendered field, holding whichever kind of state it needs.
struct FieldInput {
    field: Field,
    /// Text, secret and path fields. `None` for a toggle.
    state: Option<Entity<InputState>>,
    on: bool,
}

impl FieldInput {
    fn value(&self, cx: &App) -> String {
        match (&self.state, self.field.kind) {
            (Some(state), _) => state.read(cx).value().trim().to_string(),
            (None, FieldKind::Toggle { on, off }) => {
                if self.on {
                    on.to_string()
                } else {
                    off.to_string()
                }
            }
            (None, _) => String::new(),
        }
    }
}

pub struct ConnectionForm {
    /// `Some` when editing, so saving replaces rather than adds.
    pub editing: Option<ProfileId>,
    name: Entity<InputState>,
    scheme: &'static str,
    fields: Vec<FieldInput>,
}

impl ConnectionForm {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let service = &service::SERVICES[0];
        let mut this = Self {
            editing: None,
            name: cx
                .new(|cx| InputState::new(window, cx).placeholder(placeholders::CONNECTION_NAME)),
            scheme: service.scheme,
            fields: Vec::new(),
        };
        this.rebuild(&BTreeMap::new(), window, cx);
        this
    }

    pub fn editing(profile: &Profile, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut this = Self::new(window, cx);
        this.editing = Some(profile.id.clone());

        this.name.update(cx, |state, cx| {
            state.set_value(profile.name.clone(), window, cx)
        });

        // An unknown scheme keeps whatever it had: the form cannot render fields
        // it has no schema for, but it must not silently rewrite the profile.
        if let Some(service) = service::for_scheme(profile.scheme()) {
            this.scheme = service.scheme;
        }
        this.rebuild(&service::field_values(profile), window, cx);
        this
    }

    fn service(&self) -> &'static Service {
        service::for_scheme(self.scheme).unwrap_or(&service::SERVICES[0])
    }

    /// Recreate the inputs for the current service, seeded from `values`.
    fn rebuild(
        &mut self,
        values: &BTreeMap<String, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fields = self
            .service()
            .fields
            .iter()
            .map(|field| {
                let seeded = values
                    .get(field.key)
                    .cloned()
                    .unwrap_or_else(|| field.default_value().to_string());

                match field.kind {
                    FieldKind::Toggle { on, .. } => FieldInput {
                        field: *field,
                        state: None,
                        on: seeded == on,
                    },
                    _ => {
                        let hint = field.hint;
                        let masked = field.is_secret();
                        let state = cx.new(|cx| {
                            let mut input = InputState::new(window, cx);
                            if !hint.is_empty() {
                                input = input.placeholder(hint);
                            }
                            if masked {
                                input = input.masked(true);
                            }
                            input
                        });
                        state.update(cx, |state, cx| state.set_value(seeded, window, cx));
                        FieldInput {
                            field: *field,
                            state: Some(state),
                            on: false,
                        }
                    }
                }
            })
            .collect();
    }

    /// Preselect a service by scheme. Unknown schemes are ignored.
    pub fn select_service(&mut self, scheme: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(service) = service::for_scheme(scheme) {
            self.set_scheme(service.scheme, window, cx);
        }
    }

    /// Switch service, carrying over any field the new one also has.
    ///
    /// Keeping `endpoint` and `prefix` across an s3 → gcs switch is the whole
    /// point: they are the same values, and retyping them is exactly the tedium
    /// this form exists to remove.
    fn set_scheme(&mut self, scheme: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if self.scheme == scheme {
            return;
        }
        let carried = self.values(cx);
        self.scheme = scheme;
        self.rebuild(&carried, window, cx);
        cx.notify();
    }

    fn values(&self, cx: &App) -> BTreeMap<String, String> {
        self.fields
            .iter()
            .map(|input| (input.field.key.to_string(), input.value(cx)))
            .collect()
    }

    /// Validate and assemble the profile to save.
    pub fn build(&self, taken: &[ProfileId], cx: &App) -> Result<Profile> {
        let name = self.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            return Err(roam_core::Error::Config("请填写连接名称".into()));
        }

        let id = match &self.editing {
            Some(id) => id.clone(),
            None => profile::unique_id(&name, taken),
        };

        service::build_profile(id, name, self.scheme, &self.values(cx))
    }
}

/// How tall the scrolling field list may get, given the window's height.
///
/// The dialog itself has no height of its own to lend: gpui-component 0.5.1
/// offers `w`/`max_w` and nothing vertical, so its box grows to fit whatever it
/// is given. Its inner scroll area is `flex_1` inside that unbounded box, which
/// means it never overflows and its scrollbar never appears — the S3 form simply
/// ran off the bottom of the window with no way to reach the rest.
///
/// Sized from the viewport rather than fixed, so a short window still leaves room
/// for the title, the name and type rows, and the footer buttons. The floor keeps
/// the list usable when the window is very small; the dialog may then extend past
/// the edge, which is worse than scrolling but better than a field list two rows
/// tall.
fn fields_max_height(viewport_height: Pixels) -> Pixels {
    (viewport_height * 0.55).max(px(200.))
}

impl Render for ConnectionForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let scheme = self.scheme;

        let mut buttons = Vec::new();
        for service in service::SERVICES {
            let target = service.scheme;
            buttons.push(
                Button::new(SharedString::from(format!("svc-{target}")))
                    .label(service.label)
                    .xsmall()
                    .when(target == scheme, |b| b.primary())
                    .when(target != scheme, |b| b.outline())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.set_scheme(target, window, cx)
                    })),
            );
        }
        let picker = h_flex().gap_1().flex_wrap().children(buttons);

        // Snapshotted first: rendering a row installs a listener, which needs
        // `cx` mutably, and that cannot overlap a borrow of `self.fields`.
        let snapshot: Vec<(Field, Option<Entity<InputState>>, bool)> = self
            .fields
            .iter()
            .map(|input| (input.field, input.state.clone(), input.on))
            .collect();

        let mut rows = Vec::new();
        for (ix, (field, state, on)) in snapshot.into_iter().enumerate() {
            rows.push(Self::render_field(ix, field, state, on, cx));
        }

        let name_row = labelled(
            "名称".into(),
            None,
            Input::new(&self.name).into_any_element(),
            cx,
        );
        let type_row = labelled("类型".into(), None, picker.into_any_element(), cx);
        let note = div()
            .text_xs()
            .text_color(cx.theme().muted_foreground)
            .child(placeholders::CREDENTIAL_STORAGE_NOTE);

        // Only the field list scrolls. Name, type and the storage note stay put:
        // the first two say which connection this is, and the third is the one
        // line a person should not have to scroll to find.
        let fields = v_flex()
            .id("connection-fields")
            .gap_3()
            .max_h(fields_max_height(window.viewport_size().height))
            .overflow_y_scrollbar()
            .children(rows);

        v_flex()
            .gap_3()
            .child(name_row)
            .child(type_row)
            .child(fields)
            .child(note)
    }
}

impl ConnectionForm {
    /// Takes no `&self` on purpose — see the snapshot comment in `render`.
    fn render_field(
        ix: usize,
        field: Field,
        state: Option<Entity<InputState>>,
        on: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let label = if field.required {
            format!("{} *", field.label)
        } else {
            field.label.to_string()
        };

        let control = match (state, field.kind) {
            (Some(state), _) => Input::new(&state).into_any_element(),
            (None, FieldKind::Toggle { .. }) => Switch::new(SharedString::from(format!("tg-{ix}")))
                .checked(on)
                .on_click(cx.listener(move |this, checked: &bool, _, cx| {
                    if let Some(slot) = this.fields.get_mut(ix) {
                        slot.on = *checked;
                    }
                    cx.notify();
                }))
                .into_any_element(),
            (None, _) => div().into_any_element(),
        };

        // A toggle has no placeholder to carry its explanation, so it is the one
        // kind that still needs a hint line; text hints ride in the placeholder.
        let hint = match field.kind {
            FieldKind::Toggle { .. } => Some(field.hint),
            _ => None,
        };

        labelled(label.into(), hint, control, cx).into_any_element()
    }
}

fn labelled(
    label: SharedString,
    hint: Option<&str>,
    control: AnyElement,
    cx: &App,
) -> impl IntoElement + use<> {
    let hint = hint.filter(|h| !h.is_empty()).map(|h| h.to_string());

    v_flex()
        .gap_1()
        .child(div().text_sm().child(label))
        .child(control)
        .when_some(hint, |el, hint| {
            el.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
        })
}

#[cfg(test)]
impl ConnectionForm {
    pub(crate) fn set_name(&self, value: &str, window: &mut Window, cx: &mut App) {
        let value = value.to_string();
        self.name
            .update(cx, |state, cx| state.set_value(value, window, cx));
    }

    /// Type into a field by its schema key. Panics on an unknown key: a test
    /// that names a field the service does not have is asserting nothing.
    pub(crate) fn set_field(&self, key: &str, value: &str, window: &mut Window, cx: &mut App) {
        let input = self
            .fields
            .iter()
            .find(|f| f.field.key == key)
            .unwrap_or_else(|| panic!("no field {key} for scheme {}", self.scheme));

        let value = value.to_string();
        match &input.state {
            Some(state) => state.update(cx, |state, cx| state.set_value(value, window, cx)),
            None => panic!("{key} is a toggle; use toggle_field"),
        }
    }

    pub(crate) fn choose_scheme(
        &mut self,
        scheme: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.select_service(scheme, window, cx);
    }

    pub(crate) fn scheme(&self) -> &'static str {
        self.scheme
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_field_list_is_capped_against_the_window_not_a_fixed_size() {
        // A tall window gives the list room proportional to itself…
        assert_eq!(fields_max_height(px(1000.)), px(550.));
        // …and a short one falls back to the floor rather than to something
        // unusably small. 200 * 0.55 = 110, which would be two rows.
        assert_eq!(fields_max_height(px(200.)), px(200.));
    }

    /// Every service's field list has to be reachable. S3 is the tallest at seven
    /// fields; at roughly 56px per row that is ~390px, which is why the cap is a
    /// fraction of the viewport rather than a number that happens to fit today.
    #[test]
    fn the_cap_leaves_room_for_the_tallest_service() {
        let tallest = service::SERVICES
            .iter()
            .map(|s| s.fields.len())
            .max()
            .unwrap();
        assert!(tallest >= 7, "S3 should still be the tallest: {tallest}");

        // On the window size the app actually opens at, the cap must be enough
        // to show several rows before scrolling starts.
        let cap = fields_max_height(px(760.));
        assert!(cap > px(400.), "got {cap:?}");
    }

    /// The schema is what the form renders, so a field the form cannot draw
    /// would be invisible-but-required — the connection would simply refuse to
    /// save with no input to fix it.
    #[test]
    fn every_field_kind_has_a_control() {
        for service in service::SERVICES {
            for field in service.fields {
                match field.kind {
                    FieldKind::Text | FieldKind::Secret | FieldKind::Path => {}
                    FieldKind::Toggle { on, off } => {
                        assert!(!on.is_empty() && !off.is_empty(), "{}", field.key);
                    }
                }
            }
        }
    }
}
