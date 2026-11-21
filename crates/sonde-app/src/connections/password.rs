//! Asking for a password at connect time, for sessions with no keyring.

use gpui_kit::component::{
    WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputState},
    v_flex,
};
use gpui_kit::*;
use sonde_core::Secret;

use crate::browser::BrowserPanel;

struct PasswordPrompt {
    browser: Entity<BrowserPanel>,
    connection: String,
    password: Entity<InputState>,
}

/// Ask for the password of `connection`, then connect with it.
pub fn open_password_prompt(
    browser: Entity<BrowserPanel>,
    connection: String,
    name: String,
    window: &mut Window,
    cx: &mut App,
) {
    let prompt = cx.new(|cx| {
        let password = cx.new(|cx| InputState::new(window, cx).masked(true));
        PasswordPrompt {
            browser,
            connection,
            password,
        }
    });
    let password = prompt.read(cx).password.clone();
    window.open_dialog(cx, move |dialog, _, _| {
        let submit = prompt.clone();
        dialog
            .title(format!("Password for {name}"))
            .w(px(420.))
            .p_6()
            .on_ok(move |_, window, cx| {
                submit.update(cx, |prompt, cx| prompt.submit(window, cx));
                // Submit closes this dialog before starting the connection.
                false
            })
            .child(prompt.clone())
    });
    password.update(cx, |password, cx| password.focus(window, cx));
}

impl PasswordPrompt {
    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let secret = Secret::new(self.password.read(cx).value().to_string());
        let connection = self.connection.clone();
        window.close_dialog(cx);
        self.browser.update(cx, |browser, cx| {
            browser.connect(&connection, Some(secret), window, cx);
        });
    }
}

impl Render for PasswordPrompt {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_5()
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .child("Password"),
            )
            .child(
                Input::new(&self.password)
                    .id("prompt-password")
                    .aria_label("Password")
                    .mask_toggle(),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("cancel")
                            .ghost()
                            .h_9()
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("connect")
                            .primary()
                            .h_9()
                            .label("Connect")
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}
